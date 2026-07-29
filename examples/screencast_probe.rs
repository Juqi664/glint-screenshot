//! Probe: ScreenCast portal -> PipeWire -> grab one frame, log buffer type.
//!
//! De-risks the Stage 2 capture path before integrating into the app. Tells us
//! whether GNOME/mutter hands us a CPU-mmap'd buffer (MemFd/MemPtr) or a
//! DMA-BUF (which would need EGL import to read pixels).

use std::sync::mpsc;
use std::thread;

use anyhow::{anyhow, Result};
use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use ashpd::desktop::PersistMode;
use pipewire as pw;
use pipewire::properties::properties;
use pipewire::spa;
use pipewire::spa::buffer::DataType;
use pipewire::spa::pod::Pod;

struct UserData {
    format: spa::param::video::VideoInfoRaw,
}

fn main() -> Result<()> {
    env_logger::init();
    pw::init();

    let report = async_std::task::block_on(async {
        log::info!("Creating ScreenCast session...");
        let sc = Screencast::new()
            .await
            .map_err(|e| anyhow!("screencast new: {e:?}"))?;
        let session = sc
            .create_session()
            .await
            .map_err(|e| anyhow!("create_session: {e:?}"))?;

        log::info!("Selecting sources (a dialog may appear on first run)...");
        sc.select_sources(
            &session,
            CursorMode::Hidden,
            SourceType::Monitor | SourceType::Window,
            false,
            None,
            PersistMode::ExplicitlyRevoked,
        )
        .await
        .map_err(|e| anyhow!("select_sources: {e:?}"))?
        .response()
        .map_err(|e| anyhow!("select_sources response: {e:?}"))?;

        log::info!("Starting session...");
        let streams = sc
            .start(&session, None)
            .await
            .map_err(|e| anyhow!("start: {e:?}"))?
            .response()
            .map_err(|e| anyhow!("start response: {e:?}"))?;
        let stream = streams
            .streams()
            .first()
            .ok_or_else(|| anyhow!("no streams returned"))?;
        let node_id = stream.pipe_wire_node_id();
        log::info!("Got stream, pipewire node id = {node_id}");

        let fd = sc
            .open_pipe_wire_remote(&session)
            .await
            .map_err(|e| anyhow!("open_pipe_wire_remote: {e:?}"))?;
        log::info!("Opened PipeWire remote fd");

        // Hand the fd + node id to a PipeWire main loop running on a thread.
        let (tx, rx) = mpsc::channel::<String>();
        let handle = thread::spawn(move || -> Result<()> {
            let mainloop = pw::main_loop::MainLoopRc::new(None)?;
            let context = pw::context::ContextRc::new(&mainloop, None)?;
            let core = context.connect_fd_rc(fd, None)?;

            let stream = pw::stream::StreamRc::new(
                core,
                "glint-probe",
                properties! {
                    *pw::keys::MEDIA_TYPE => "Video",
                    *pw::keys::MEDIA_CATEGORY => "Capture",
                },
            )?;

            let data = UserData {
                format: spa::param::video::VideoInfoRaw::new(),
            };

            // Offer several BGRA-ish formats + a wide size range.
            let obj = pw::spa::pod::object!(
                pw::spa::utils::SpaTypes::ObjectParamFormat,
                pw::spa::param::ParamType::EnumFormat,
                pw::spa::pod::property!(
                    pw::spa::param::format::FormatProperties::MediaType,
                    Id,
                    pw::spa::param::format::MediaType::Video
                ),
                pw::spa::pod::property!(
                    pw::spa::param::format::FormatProperties::MediaSubtype,
                    Id,
                    pw::spa::param::format::MediaSubtype::Raw
                ),
                pw::spa::pod::property!(
                    pw::spa::param::format::FormatProperties::VideoFormat,
                    Choice,
                    Enum,
                    Id,
                    pw::spa::param::video::VideoFormat::BGRA,
                    pw::spa::param::video::VideoFormat::BGRx,
                    pw::spa::param::video::VideoFormat::RGBA,
                    pw::spa::param::video::VideoFormat::RGBx,
                    pw::spa::param::video::VideoFormat::RGB,
                ),
                pw::spa::pod::property!(
                    pw::spa::param::format::FormatProperties::VideoSize,
                    Choice,
                    Range,
                    Rectangle,
                    pw::spa::utils::Rectangle {
                        width: 320,
                        height: 240
                    },
                    pw::spa::utils::Rectangle {
                        width: 1,
                        height: 1
                    },
                    pw::spa::utils::Rectangle {
                        width: 16384,
                        height: 16384
                    }
                ),
                pw::spa::pod::property!(
                    pw::spa::param::format::FormatProperties::VideoFramerate,
                    Choice,
                    Range,
                    Fraction,
                    pw::spa::utils::Fraction { num: 0, denom: 1 },
                    pw::spa::utils::Fraction { num: 0, denom: 1 },
                    pw::spa::utils::Fraction {
                        num: 1000,
                        denom: 1
                    }
                ),
            );
            let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
                std::io::Cursor::new(Vec::new()),
                &pw::spa::pod::Value::Object(obj),
            )
            .map_err(|e| anyhow!("serialize pod: {e:?}"))?
            .0
            .into_inner();
            let mut param_pod =
                Pod::from_bytes(&values).ok_or_else(|| anyhow!("Pod::from_bytes returned None"))?;

            let mainloop_for_cb = mainloop.clone();
            let _listener = stream
                .add_local_listener_with_user_data(data)
                .state_changed(|_, _, old, new| {
                    log::info!("pw stream state: {old:?} -> {new:?}");
                })
                .param_changed(|_, ud, id, param| {
                    let Some(param) = param else {
                        return;
                    };
                    if id != pw::spa::param::ParamType::Format.as_raw() {
                        return;
                    }
                    let (mt, ms) = match pw::spa::param::format_utils::parse_format(param) {
                        Ok(v) => v,
                        Err(_) => return,
                    };
                    if mt != pw::spa::param::format::MediaType::Video
                        || ms != pw::spa::param::format::MediaSubtype::Raw
                    {
                        return;
                    }
                    if ud.format.parse(param).is_ok() {
                        log::info!(
                            "negotiated format: {:?} size {}x{} framerate {}/{}",
                            ud.format.format(),
                            ud.format.size().width,
                            ud.format.size().height,
                            ud.format.framerate().num,
                            ud.format.framerate().denom
                        );
                    }
                })
                .process(move |stream, _ud| {
                    if let Some(mut buffer) = stream.dequeue_buffer() {
                        let datas = buffer.datas_mut();
                        if datas.is_empty() {
                            let _ = tx.send("buffer: no datas".into());
                            return;
                        }
                        let d = &datas[0];
                        let chunk = d.chunk();
                        let ty = d.type_();
                        let ty_name = match ty {
                            DataType::MemPtr => "MemPtr",
                            DataType::MemFd => "MemFd",
                            DataType::DmaBuf => "DmaBuf",
                            _ => "Other",
                        };
                        let msg = format!(
                            "type={ty_name} chunk_size={} offset={} stride={}",
                            chunk.size(),
                            chunk.offset(),
                            chunk.stride()
                        );
                        log::info!("GOT BUFFER: {msg}");
                        let _ = tx.send(msg);
                        mainloop_for_cb.quit();
                    }
                })
                .register()?;

            stream.connect(
                spa::utils::Direction::Input,
                Some(node_id),
                pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
                std::slice::from_mut(&mut param_pod),
            )?;
            log::info!("pw stream connected, running main loop...");
            mainloop.run();
            log::info!("pw main loop exited");
            // keep listener alive
            drop(_listener);
            Ok(())
        });

        // Wait for the first frame report (or a timeout).
        let msg = match rx.recv_timeout(std::time::Duration::from_secs(20)) {
            Ok(msg) => {
                log::info!("FIRST FRAME REPORT: {msg}");
                msg
            }
            Err(_) => {
                log::error!("Timed out waiting for a frame");
                String::from("TIMEOUT")
            }
        };
        drop(rx);
        let _ = handle.join();
        Ok::<String, anyhow::Error>(msg)
    })?;

    println!("FIRST FRAME REPORT: {report}");
    Ok(())
}
