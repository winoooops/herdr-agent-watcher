fn main() {
    env_logger::init();
    let mode = std::env::args().nth(1).unwrap_or_default();
    let code = match mode.as_str() {
        "daemon" => herdr_agent_watcher::daemon::run::run(),
        "sidebar" => herdr_agent_watcher::sidebar::tui::run(),
        "sidebar-open" => {
            let client = herdr_agent_watcher::herdr::client::HerdrClient::from_env();
            let plugin_id =
                std::env::var("HERDR_PLUGIN_ID").unwrap_or_else(|_| "herdr-agent-watcher".into());
            match client.plugin_pane_open(&plugin_id, "sidebar") {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("open sidebar failed: {error}");
                    1
                }
            }
        }
        "stop" => herdr_agent_watcher::daemon::singleton::stop(),
        "kimi-consent" => {
            use herdr_agent_watcher::agents::consent;
            let path = consent::consent_path();
            match std::env::args().nth(2).as_deref() {
                Some("on") => consent::set_and_persist(&path, true)
                    .map(|_| 0)
                    .unwrap_or(1),
                Some("off") => consent::set_and_persist(&path, false)
                    .map(|_| 0)
                    .unwrap_or(1),
                Some("status") => {
                    consent::load_into_memory(&path);
                    let enabled = consent::enabled();
                    println!("{}", if enabled { "enabled" } else { "disabled" });
                    i32::from(!enabled)
                }
                _ => {
                    eprintln!("usage: herdr-agent-watcher kimi-consent <on|off|status>");
                    2
                }
            }
        }
        "claude-bridge" => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            herdr_agent_watcher::agents::claude_bridge::cli_claude_bridge(&args)
        }
        "generate-scripts" => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            herdr_agent_watcher::agents::claude_bridge::cli_generate_scripts(&args)
        }
        "enable-claude-bridge" => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            herdr_agent_watcher::agents::claude_bridge::cli_enable(&args)
        }
        "disable-claude-bridge" => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            herdr_agent_watcher::agents::claude_bridge::cli_disable(&args)
        }
        "doctor" => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            herdr_agent_watcher::agents::claude_bridge::cli_doctor(&args)
        }
        "bind-sidebar-key" => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            herdr_agent_watcher::agents::keybinding::cli_bind(&args)
        }
        "unbind-sidebar-key" => {
            let args: Vec<String> = std::env::args().skip(2).collect();
            herdr_agent_watcher::agents::keybinding::cli_unbind(&args)
        }
        other => {
            eprintln!(
                "usage: herdr-agent-watcher <daemon|sidebar|sidebar-open|stop|kimi-consent|claude-bridge|enable-claude-bridge|disable-claude-bridge|doctor|bind-sidebar-key|unbind-sidebar-key> (got {other:?})"
            );
            2
        }
    };
    std::process::exit(code);
}
