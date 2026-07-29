use gtk4::prelude::*;
use gtk4::IconTheme;

fn main() {
    let app = gtk4::Application::builder()
        .application_id("io.github.glint.iconprobe")
        .build();
    app.connect_activate(move |a| {
        let disp = gtk4::gdk::Display::default().unwrap();
        let theme = IconTheme::for_display(&disp);
        for name in [
            "glint-tool-rect-symbolic",
            "glint-tool-ellipse-symbolic",
            "glint-tool-line-symbolic",
            "glint-tool-arrow-symbolic",
            "glint-tool-brush-symbolic",
            "glint-tool-mosaic-symbolic",
            "edit-select-symbolic",
        ] {
            let has = theme.has_icon(name);
            println!("{name}: has_icon={has}");
        }
        a.quit();
    });
    app.run();
}
