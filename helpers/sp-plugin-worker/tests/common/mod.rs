//! Checks shared by the worker end-to-end tests.
#![cfg(target_os = "linux")]

/// Finds the worker's editor window for rack 0, slot 0 on the private display and checks what
/// the window streamer reads: the owning process, rack, slot, and title.
pub fn assert_editor_window_identity(display: &sp_supervisor::PrivateDisplay, worker: u32) {
    use xcb::x;

    let authority = std::fs::read(display.authority()).expect("read display authority");
    let cookie = std::str::from_utf8(&authority[authority.len() - 16..]).expect("text cookie");
    let (connection, screen) = xcb::Connection::connect_to_display_with_auth_info(
        Some(&display.name()),
        xcb::AuthInfo {
            name: "MIT-MAGIC-COOKIE-1",
            data: cookie,
        },
    )
    .expect("connect to the private display");
    let root = connection
        .get_setup()
        .roots()
        .nth(usize::try_from(screen).expect("screen index"))
        .expect("screen")
        .root();
    let atom = |name: &str| {
        connection
            .wait_for_reply(connection.send_request(&x::InternAtom {
                only_if_exists: false,
                name: name.as_bytes(),
            }))
            .expect("intern atom")
            .atom()
    };
    let (pid, rack, slot, name, utf8) = (
        atom("_NET_WM_PID"),
        atom("_SUPERPOSITION_RACK"),
        atom("_SUPERPOSITION_SLOT"),
        atom("_NET_WM_NAME"),
        atom("UTF8_STRING"),
    );
    let cardinal = |window, property| {
        connection
            .wait_for_reply(connection.send_request(&x::GetProperty {
                delete: false,
                window,
                property,
                r#type: x::ATOM_CARDINAL,
                long_offset: 0,
                long_length: 1,
            }))
            .expect("read window property")
            .value::<u32>()
            .first()
            .copied()
    };
    let title = |window| {
        let reply = connection
            .wait_for_reply(connection.send_request(&x::GetProperty {
                delete: false,
                window,
                property: name,
                r#type: utf8,
                long_offset: 0,
                long_length: 256,
            }))
            .expect("read window title");
        String::from_utf8_lossy(reply.value::<u8>()).into_owned()
    };
    let windows = connection
        .wait_for_reply(connection.send_request(&x::QueryTree { window: root }))
        .expect("list top-level windows");
    assert!(
        windows.children().iter().any(|&window| {
            cardinal(window, pid) == Some(worker)
                && cardinal(window, rack) == Some(0)
                && cardinal(window, slot) == Some(0)
                && title(window).ends_with(" – Rack 1, Slot 1")
        }),
        "no top-level window carries the worker's PID, rack 0, slot 0, and its title"
    );
}
