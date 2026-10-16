//! cad-render-mcp: an MCP server (stdio) that renders OpenCascade-readable CAD files on the GPU.

mod camera;
mod font;
mod occt;
mod photo;
mod project;
mod render;
mod scene;
mod server;
mod templates;
mod video;

use rmcp::ServiceExt;
use std::os::fd::FromRawFd;

/// stdout carries MCP JSON-RPC. Move it to a private fd and point fd 1 at stderr, so stray
/// prints from native libraries (OCCT, Metal) can never corrupt the protocol stream.
fn protocol_stdout() -> std::fs::File {
    unsafe {
        let fd = libc::dup(1);
        assert!(fd >= 0, "dup(stdout) failed");
        libc::dup2(2, 1);
        std::fs::File::from_raw_fd(fd)
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version" | "-V") => {
            println!(
                "cad-render-mcp {} (OCCT {})",
                env!("CARGO_PKG_VERSION"),
                occt::occt_version()
            );
            Ok(())
        }
        Some("bench") => bench(&args[1..]),
        Some("demo-step") => {
            let out = args.get(1).map(String::as_str).unwrap_or("demo_rack.step");
            occt::write_demo_step(out)?;
            println!("wrote {out}");
            Ok(())
        }
        Some("--help" | "-h") => {
            println!(
                "cad-render-mcp {}: MCP server (stdio) for rendering and inspecting STEP/BREP/IGES files.\n\
                 Register with: claude mcp add cad-render -- {}\n\
                 Dev: cad-render-mcp bench <file|dir>... [-o out.png] | cad-render-mcp demo-step [out.step]",
                env!("CARGO_PKG_VERSION"),
                std::env::current_exe()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            );
            Ok(())
        }
        _ => serve(),
    }
}

fn serve() -> anyhow::Result<()> {
    let out = protocol_stdout();
    let app = server::App::new();
    if let Some(e) = &app.renderer_err {
        eprintln!("[cad-render] {e}");
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let transport = (tokio::io::stdin(), tokio::fs::File::from_std(out));
        let service = server::CadRender::new(app).serve(transport).await?;
        service.waiting().await?;
        anyhow::Ok(())
    })
}

/// Developer benchmark: load, then time a few typical renders.
fn bench(args: &[String]) -> anyhow::Result<()> {
    let mut paths = Vec::new();
    let mut out = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "-o" {
            out = it.next().cloned();
        } else {
            paths.push(a.clone());
        }
    }
    let t = std::time::Instant::now();
    let mut app = server::App::new();
    println!(
        "gpu init {:.0} ms ({})",
        t.elapsed().as_secs_f64() * 1e3,
        app.renderer
            .as_ref()
            .map(|r| r.adapter_name.as_str())
            .unwrap_or("none")
    );
    let s = server::do_open(
        &mut app,
        &server::OpenParams {
            paths,
            add: None,
            deflection_mm: None,
            angle_deg: None,
            split_solids: std::env::var("SPLIT").ok().map(|v| v == "1"),
            project: None,
        },
    )?;
    println!(
        "{}",
        s.lines()
            .take_while(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    );
    let cases: &[(&str, serde_json::Value)] = &[
        ("iso 1200x900", serde_json::json!({})),
        ("iso again (warm)", serde_json::json!({})),
        (
            "4 views 800x600",
            serde_json::json!({"views": ["iso", "front", "right", "top"]}),
        ),
        (
            "section x",
            serde_json::json!({"views": ["iso"], "section": {"axis": "x"}}),
        ),
        (
            "6 views 640x480 + labels",
            serde_json::json!({"views": ["iso", "iso_bl", "front", "back", "left", "top"], "part_labels": true}),
        ),
    ];
    for (name, v) in cases {
        let p: server::RenderParams = serde_json::from_value(v.clone())?;
        let t = std::time::Instant::now();
        let r = server::do_render(&mut app, &p)?;
        println!(
            "{name:28} {:6.1} ms  {}",
            t.elapsed().as_secs_f64() * 1e3,
            r.text.lines().next().unwrap_or("")
        );
        if let Some(o) = &out {
            let file = o.replace(
                ".png",
                &format!("_{}.png", name.split_whitespace().next().unwrap_or("x")),
            );
            std::fs::write(file, &r.png)?;
        }
    }
    Ok(())
}
