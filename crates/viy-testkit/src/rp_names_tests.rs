use super::*;

#[test]
fn the_map_is_read() {
    let doc: Value = serde_json::from_str(MAP).unwrap();
    assert_eq!(pairs().len(), doc["map"].as_array().unwrap().len());
    assert!(pairs().len() >= 15);
}

#[test]
fn rp_names_become_viys() {
    assert_eq!(
        to_viy("[rpctl: tool-call budget] [rp chat: ssh: x] X-Rpctl-Blocked"),
        "[viy: tool-call budget] [viy chat: ssh: x] X-Proxy-Blocked"
    );
    assert_eq!(
        value_to_viy(&serde_json::json!({"rpctl_note": ["/run/rpctl/host.git"]})),
        serde_json::json!({"viy_note": ["/run/viy/host.git"]})
    );
}

#[test]
fn what_leaves_the_machine_is_not_in_the_map() {
    // ~/rp-work on a lent server keeps rp's name (kb d131).
    assert_eq!(
        to_viy("mkdir -p \"$HOME\"/rp-work"),
        "mkdir -p \"$HOME\"/rp-work"
    );
}

#[test]
fn a_response_gets_viys_names_and_its_new_length() {
    let rp = b"HTTP/1.1 403 Forbidden\r\nX-Rpctl-Blocked: no\r\nContent-Length: 23\r\n\r\nrpctl egress proxy: no\n";
    assert_eq!(
        http_response_to_viy(rp),
        b"HTTP/1.1 403 Forbidden\r\nX-Proxy-Blocked: no\r\nContent-Length: 21\r\n\r\nviy egress proxy: no\n"
    );
    assert_eq!(http_response_to_viy(b"\xff"), b"\xff");
}
