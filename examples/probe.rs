fn main() {
    let client = agent_watcher::herdr::client::HerdrClient::from_env();
    println!("{:#?}", client.pane_list().unwrap());
    client
        .notification_show("agent-watcher", "probe: hello from the plugin")
        .unwrap();
}
