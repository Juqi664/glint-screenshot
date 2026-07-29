//! Fast capture backend: XDG Desktop Portal **ScreenCast** + PipeWire.
//!
//! Unlike the `Screenshot` portal (which makes GNOME encode a full-desktop PNG
//! to disk, ~600ms), the ScreenCast portal hands us a live PipeWire stream.
//! On GNOME/mutter the negotiated buffer is a **MemFd** (mmap'd CPU memory) in
//! `BGRx`/`BGRA` format, so we can read pixels directly — no DMA-BUF / EGL
//! import is needed.
//!
//! To replicate the full virtual desktop (the Screenshot portal returns one
//! combined image spanning all monitors), we open the session with
//! `multiple = true` and grab one frame from every monitor stream, then stitch
//! them by compositor position into a single Cairo `ARGB32` `ImageSurface`.
//!
//! First run shows a one-time "select what to share" dialog; we persist the
//! portal `restore_token` so subsequent runs skip the dialog.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};

use anyhow::{anyhow, Result};
use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
use ashpd::desktop::PersistMode;
use cairo::ImageSurface;
use enumflags2::BitFlags;
use pipewire as pw;
use pipewire::properties::properties;
use pipewire::spa;
use pipewire::spa::pod::Pod;

/// Per-monitor info we need from the portal.
pub struct StreamInfo {
    node_id: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

/// A captured frame: raw `BGRx`/`BGRA` bytes + row stride.
pub struct Frame {
    stride: usize,
    w: i32,
    h: i32,
    data: Vec<u8>,
}

/// Listener user data holding the negotiated video format.
struct FrameUd {
    format: spa::param::video::VideoInfoRaw,
}

/// Where to persist the portal restore token between runs.
fn token_path() -> PathBuf {
    let mut p = dirs_cache();
    p.push("glint-screencast-token");
    p
}

fn dirs_cache() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            home.join(".cache")
        })
}

fn load_restore_token() -> Option<String> {
    fs::read_to_string(token_path())
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string())
}

fn save_restore_token(token: Option<&str>) {
    if let Some(t) = token {
        let _ = fs::create_dir_all(dirs_cache());
        let _ = fs::write(token_path(), t);
    }
}

/// Capture the full virtual desktop via the ScreenCast portal + PipeWire.
///
/// Returns the per-monitor raw frames (CPU-readable, `Send`) plus their layout.
/// The caller stitches them into a Cairo surface on the GUI thread (Cairo
/// surfaces are not `Send`). Falls back is handled by the caller.
///
/// NOTE: this must be driven from a real `async-std` runtime (e.g. inside
/// `async_std::task::block_on` on a dedicated thread), NOT from glib's
/// `spawn_future_local` executor — otherwise the portal/pipewire async I/O
/// does not progress and mutter never sends the Format param.
pub async fn capture_desktop_raw() -> Result<CapturedRaw> {
    pw::init();

    let restore_token = load_restore_token();
    log::info!(
        "ScreenCast: creating session (restore_token={}present)",
        if restore_token.is_some() { "" } else { "ab" }
    );

    let sc = Screencast::new()
        .await
        .map_err(|e| anyhow!("screencast new: {e:?}"))?;
    let session = sc
        .create_session()
        .await
        .map_err(|e| anyhow!("create_session: {e:?}"))?;

    sc.select_sources(
        &session,
        CursorMode::Hidden,
        BitFlags::from(SourceType::Monitor) | BitFlags::from(SourceType::Window),
        false, // single source: GNOME's portal refuses to connect streams
        // created from a `multiple=true` session (nodes go
        // Connecting -> Unconnected), so we capture one monitor per
        // invocation. The first-run dialog persists the choice via the
        // restore token; subsequent runs skip the dialog and are fast.
        restore_token.as_deref(),
        PersistMode::ExplicitlyRevoked,
    )
    .await
    .map_err(|e| anyhow!("select_sources: {e:?}"))?
    .response()
    .map_err(|e| anyhow!("select_sources response: {e:?}"))?;

    let started = sc
        .start(&session, None)
        .await
        .map_err(|e| anyhow!("start: {e:?}"))?
        .response()
        .map_err(|e| anyhow!("start response: {e:?}"))?;
    save_restore_token(started.restore_token());

    let mut streams: Vec<StreamInfo> = Vec::new();
    for s in started.streams() {
        let node_id = s.pipe_wire_node_id();
        let (x, y) = s.position().unwrap_or((0, 0));
        let (w, h) = s.size().unwrap_or((0, 0));
        log::info!("ScreenCast stream node={node_id} pos=({x},{y}) size={w}x{h}");
        if w > 0 && h > 0 {
            streams.push(StreamInfo {
                node_id,
                x,
                y,
                w,
                h,
            });
        }
    }
    if streams.is_empty() {
        return Err(anyhow!("ScreenCast returned no usable streams"));
    }

    let fd = sc
        .open_pipe_wire_remote(&session)
        .await
        .map_err(|e| anyhow!("open_pipe_wire_remote: {e:?}"))?;
    log::info!("ScreenCast: opened PipeWire remote, grabbing frames...");

    let frames = grab_frames(fd, &streams).await?;
    Ok(CapturedRaw { streams, frames })
}

/// Per-monitor raw capture result (all `Send`), to be stitched on the GUI thread.
pub struct CapturedRaw {
    pub streams: Vec<StreamInfo>,
    pub frames: HashMap<u32, Frame>,
}

/// Stitch per-monitor frames into one combined `ARGB32` surface. Must be called
/// on the GUI thread (Cairo surfaces are not `Send`).
pub fn stitch(captured: CapturedRaw) -> Result<ImageSurface> {
    stitch_frames(&captured.streams, &captured.frames)
}

/// Connect a PipeWire stream per monitor, grab the first frame of each, return
/// them keyed by node id.
///
/// The PipeWire main loop runs on a background thread; we poll the done signal
/// with `glib::timeout_future` so the GTK main loop keeps iterating (the
/// loading overlay keeps rendering) while we wait.
async fn grab_frames(
    fd: std::os::fd::OwnedFd,
    streams: &[StreamInfo],
) -> Result<HashMap<u32, Frame>> {
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let collected: Arc<Mutex<HashMap<u32, Frame>>> = Arc::new(Mutex::new(HashMap::new()));

    let streams_vec: Vec<StreamInfo> = streams
        .iter()
        .map(|s| StreamInfo {
            node_id: s.node_id,
            x: s.x,
            y: s.y,
            w: s.w,
            h: s.h,
        })
        .collect();

    let collected_h = collected.clone();
    let handle = std::thread::spawn(move || -> Result<()> {
        log::info!("pw thread started");
        let mainloop = pw::main_loop::MainLoopRc::new(None)?;
        let context = pw::context::ContextRc::new(&mainloop, None)?;
        let core = context.connect_fd_rc(fd, None)?;

        // Build a format pod offering BGRx / BGRA (mutter picks BGRx).
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

        // One PipeWire stream per monitor. Each connects to its own screencast
        // node id (NOT PW_ID_ANY — using None makes mutter send only Props and
        // never the negotiated Format, so the stream stalls in Paused).
        let expected = streams_vec.len();
        let mut listeners: Vec<pw::stream::StreamListener<FrameUd>> = Vec::new();

        for si in &streams_vec {
            let node_id = si.node_id;
            let w = si.w;
            let h = si.h;

            let stream = pw::stream::StreamRc::new(
                core.clone(),
                "glint",
                properties! {
                    *pw::keys::MEDIA_TYPE => "Video",
                    *pw::keys::MEDIA_CATEGORY => "Capture",
                },
            )?;

            let ud = FrameUd {
                format: spa::param::video::VideoInfoRaw::new(),
            };
            let collected_c = collected_h.clone();
            let done_c = done_tx.clone();
            let mainloop_c = mainloop.clone();

            let listener = stream
                .add_local_listener_with_user_data(ud)
                .state_changed(move |_, _, old, new| {
                    log::debug!("pw stream {node_id} state: {old:?} -> {new:?}");
                })
                .param_changed(move |stream, ud, id, param| {
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
                            "pw stream {node_id} negotiated {:?} {}x{}",
                            ud.format.format(),
                            ud.format.size().width,
                            ud.format.size().height
                        );
                        let _ = stream.set_active(true);
                    }
                })
                .process(move |stream, _ud| {
                    if collected_c.lock().unwrap().contains_key(&node_id) {
                        return;
                    }
                    if let Some(mut buffer) = stream.dequeue_buffer() {
                        let datas = buffer.datas_mut();
                        if let Some(d) = datas.first_mut() {
                            let chunk = d.chunk();
                            let stride = chunk.stride() as usize;
                            let size = chunk.size() as usize;
                            if let Some(bytes) = d.data() {
                                let take = size.min(bytes.len());
                                let frame = Frame {
                                    stride,
                                    w,
                                    h,
                                    data: bytes[..take].to_vec(),
                                };
                                let mut map = collected_c.lock().unwrap();
                                if !map.contains_key(&node_id) {
                                    map.insert(node_id, frame);
                                    log::info!("pw stream {node_id} frame captured");
                                    if map.len() >= expected {
                                        let _ = done_c.send(());
                                        mainloop_c.quit();
                                    }
                                }
                            }
                        }
                    }
                })
                .register()?;

            // Each stream needs its own pod instance (connect borrows it). The
            // pod is a `&Pod` borrowing `values`, which lives until the end of
            // this closure, so it stays valid through `mainloop.run()`.
            let mut pod =
                Pod::from_bytes(&values).ok_or_else(|| anyhow!("Pod::from_bytes None"))?;
            stream.connect(
                spa::utils::Direction::Input,
                Some(node_id),
                pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
                std::slice::from_mut(&mut pod),
            )?;
            listeners.push(listener);
        }

        mainloop.run();
        log::info!("pw mainloop exited");
        // keep listeners + streams alive until after the loop ends
        drop(listeners);
        Ok(())
    });

    // Poll the done signal without blocking. We run under an async-std runtime
    // (this whole capture is executed via async_std::task::block_on on a
    // dedicated thread), so async_std::task::sleep drives the wait.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if done_rx.try_recv().is_ok() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            return Err(anyhow!("timed out waiting for screencast frames"));
        }
        async_std::task::sleep(std::time::Duration::from_millis(20)).await;
    }
    let join_res = handle.join().map_err(|_| anyhow!("pw thread panicked"))?;
    join_res?;

    let map = Arc::try_unwrap(collected).map_err(|_| anyhow!("collected still shared"))?;
    Ok(map.into_inner().unwrap())
}

/// Stitch per-monitor frames into one combined `ARGB32` surface.
fn stitch_frames(streams: &[StreamInfo], frames: &HashMap<u32, Frame>) -> Result<ImageSurface> {
    // Bounding box of the virtual desktop (compositor coords, assumed 1:1 px).
    let mut canvas_w: i32 = 0;
    let mut canvas_h: i32 = 0;
    for s in streams {
        canvas_w = canvas_w.max(s.x + s.w);
        canvas_h = canvas_h.max(s.y + s.h);
    }
    if canvas_w <= 0 || canvas_h <= 0 {
        return Err(anyhow!("invalid desktop size {canvas_w}x{canvas_h}"));
    }
    log::info!("ScreenCast: stitching into {canvas_w}x{canvas_h}");

    let mut surface = ImageSurface::create(cairo::Format::ARgb32, canvas_w, canvas_h)
        .map_err(|e| anyhow!("create canvas: {e}"))?;
    let stride = surface.stride() as usize;

    {
        let mut data = surface.data().map_err(|e| anyhow!("borrow canvas: {e}"))?;
        let buf: &mut [u8] = &mut *data;
        // Clear to opaque black.
        for b in buf.iter_mut() {
            *b = 0;
        }
        for s in streams {
            let frame = match frames.get(&s.node_id) {
                Some(f) => f,
                None => {
                    log::warn!("no frame for node {}, skipping", s.node_id);
                    continue;
                }
            };
            // Blit BGRx -> ARGB32 (set alpha=255) at (s.x, s.y).
            let src_stride = frame.stride;
            let dst_x = s.x as usize;
            let dst_y = s.y as usize;
            let w = (s.w as usize).min(frame.w as usize);
            let h = (s.h as usize).min(frame.h as usize);
            for y in 0..h {
                let src_row = y * src_stride;
                let dst_row = (dst_y + y) * stride + dst_x * 4;
                for x in 0..w {
                    let sp = src_row + x * 4;
                    let dp = dst_row + x * 4;
                    if sp + 3 < frame.data.len() && dp + 3 < buf.len() {
                        buf[dp] = frame.data[sp]; // B
                        buf[dp + 1] = frame.data[sp + 1]; // G
                        buf[dp + 2] = frame.data[sp + 2]; // R
                        buf[dp + 3] = 0xFF; // A
                    }
                }
            }
        }
    }
    surface.mark_dirty();
    Ok(surface)
}
