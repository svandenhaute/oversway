//! Test helper: inject key events through zwp_virtual_keyboard_v1.
//! Usage: keys "56:1 56:0 sleep:200 ..." (evdev code:1 press / :0 release, sleep:ms).
use std::io::Write;
use std::os::fd::AsFd;
use wayland_client::{globals::{registry_queue_init, GlobalListContents}, protocol::{wl_registry, wl_seat}, Connection, Dispatch, QueueHandle};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1, zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1};

struct State;
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State { fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {} }
impl Dispatch<wl_seat::WlSeat, ()> for State { fn event(_: &mut Self, _: &wl_seat::WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {} }
impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for State { fn event(_: &mut Self, _: &ZwpVirtualKeyboardManagerV1, _: <ZwpVirtualKeyboardManagerV1 as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {} }
impl Dispatch<ZwpVirtualKeyboardV1, ()> for State { fn event(_: &mut Self, _: &ZwpVirtualKeyboardV1, _: <ZwpVirtualKeyboardV1 as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {} }

fn main() {
    let script = std::env::args().nth(1).expect("key script");
    let connection = Connection::connect_to_env().unwrap();
    let (globals, mut queue) = registry_queue_init::<State>(&connection).unwrap();
    let handle = queue.handle();
    let seat: wl_seat::WlSeat = globals.bind(&handle, 1..=7, ()).unwrap();
    let manager: ZwpVirtualKeyboardManagerV1 = globals.bind(&handle, 1..=1, ()).unwrap();
    let keyboard = manager.create_virtual_keyboard(&seat, &handle, ());

    let context = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
    let keymap = xkbcommon::xkb::Keymap::new_from_names(&context, "", "", "us", "", None, xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS).unwrap();
    let mut text = keymap.get_as_string(xkbcommon::xkb::KEYMAP_FORMAT_TEXT_V1).into_bytes();
    text.push(0);
    let mut file = tempfile();
    file.write_all(&text).unwrap();
    keyboard.keymap(1, file.as_fd(), text.len() as u32);
    let mut state = xkbcommon::xkb::State::new(&keymap);
    queue.roundtrip(&mut State).unwrap();

    let start = std::time::Instant::now();
    for step in script.split_whitespace() {
        let (left, right) = step.split_once(':').unwrap();
        if left == "sleep" {
            std::thread::sleep(std::time::Duration::from_millis(right.parse().unwrap()));
            continue;
        }
        let code: u32 = left.parse().unwrap();
        let pressed = right == "1";
        let time = start.elapsed().as_millis() as u32;
        keyboard.key(time, code, pressed as u32);
        state.update_key((code + 8).into(), if pressed { xkbcommon::xkb::KeyDirection::Down } else { xkbcommon::xkb::KeyDirection::Up });
        keyboard.modifiers(
            state.serialize_mods(xkbcommon::xkb::STATE_MODS_DEPRESSED),
            state.serialize_mods(xkbcommon::xkb::STATE_MODS_LATCHED),
            state.serialize_mods(xkbcommon::xkb::STATE_MODS_LOCKED),
            state.serialize_layout(xkbcommon::xkb::STATE_LAYOUT_EFFECTIVE),
        );
        queue.roundtrip(&mut State).unwrap();
    }
}

fn tempfile() -> std::fs::File {
    let directory = std::env::var("XDG_RUNTIME_DIR").unwrap();
    let path = format!("{directory}/oversway-keys-{}", std::process::id());
    let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).unwrap();
    std::fs::remove_file(path).unwrap();
    file
}
