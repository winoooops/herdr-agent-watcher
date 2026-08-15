fn main() {
    let client = herdr_agent_watcher::herdr::client::HerdrClient::from_env();
    println!("{:#?}", client.pane_list().unwrap());
    client
        .notification_show("herdr-agent-watcher", "probe: hello from the plugin")
        .unwrap();
}
