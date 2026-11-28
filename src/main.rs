use colab_mcp::mcp::coordinator::ColabCoordinator;
use colab_mcp::mcp::server::serve_stdio;
use std::env;
use std::path::PathBuf;
use std::process;
use std::time::Instant;

fn default_data_dir() -> Option<PathBuf> {
    if let Ok(d) = env::var("COLAB_MCP_DATA_DIR") {
        let t = d.trim();
        if !t.is_empty() {
            return Some(PathBuf::from(t));
        }
    }
    env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|home| PathBuf::from(home).join(".local/share/colab-mcp"))
}

fn usage() -> ! {
    eprintln!(
        "colab-mcp v{}\nUsage:\n  colab-mcp serve [--data-dir DIR] [--panel-html FILE] [--mock|--real]\n  colab-mcp doctor [--data-dir DIR] [--mock]\n  colab-mcp status [--data-dir DIR] [--mock]\n  colab-mcp --version",
        env!("CARGO_PKG_VERSION")
    );
    process::exit(2);
}

fn main() {
    let mut args = env::args().skip(1);
    let Some(cmd) = args.next() else {
        usage();
    };
    if matches!(cmd.as_str(), "--version" | "-V" | "version") {
        println!("colab-mcp {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if matches!(cmd.as_str(), "--help" | "-h" | "help") {
        usage();
    }

    let mut data_dir: Option<PathBuf> = None;
    let mut panel_html: Option<PathBuf> = None;
    let mut force_mock: Option<bool> = match env::var("COLAB_MCP_BACKEND").ok().as_deref() {
        Some("mock") => Some(true),
        Some("real") => Some(false),
        _ => None,
    };

    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--data-dir" => {
                let Some(val) = args.next() else { usage() };
                if val.trim().is_empty() {
                    usage();
                }
                data_dir = Some(PathBuf::from(val));
            }
            "--panel-html" => {
                let Some(val) = args.next() else { usage() };
                panel_html = Some(PathBuf::from(val));
            }
            "--mock" => force_mock = Some(true),
            "--real" => force_mock = Some(false),
            _ => usage(),
        }
    }

    let Some(data_dir) = data_dir.or_else(default_data_dir) else {
        eprintln!(
            "colab-mcp error: cannot determine data directory; set HOME, COLAB_MCP_DATA_DIR, or pass --data-dir"
        );
        process::exit(1);
    };

    match cmd.as_str() {
        "serve" => {
            if let Err(e) = serve_stdio(data_dir, panel_html, force_mock) {
                eprintln!("colab-mcp serve error: {e}");
                process::exit(1);
            }
        }
        "doctor" => {
            let coord = ColabCoordinator::new(data_dir, force_mock);
            let env = coord.refresh_env();
            coord.shutdown();
            match env {
                Ok(env) => println!("{}", serde_json::to_string_pretty(&env).unwrap_or_default()),
                Err(error) => {
                    eprintln!("colab-mcp doctor error: {error}");
                    process::exit(1);
                }
            }
        }
        "status" => {
            let coord = ColabCoordinator::new(data_dir, force_mock);
            let _ = coord.refresh(true);
            let status = coord.cached_snapshot().public_json(Instant::now());
            coord.shutdown();
            println!(
                "{}",
                serde_json::to_string_pretty(&status).unwrap_or_default()
            );
        }
        _ => usage(),
    }
}
