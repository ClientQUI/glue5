//! Game files changed on their way to the engine, where its code is a later
//! revision than the game's data.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

type Patch = fn(String) -> Option<String>;

/// The files (under the game's `data/`), whether this run changes them, and
/// how.
const OVERLAYS: [(&str, fn() -> bool, Patch); 3] = [
    ("common/data/gameconfig.xml", online, crate::online::more_broadcast_data),
    ("common/data/ui/pausemenu.xml", always, autosave_first),
    ("common/data/ui/frontend.xml", always, feed_width),
];

fn online() -> bool {
    crate::rt().config.online.is_some()
}

fn always() -> bool {
    true
}

/// Where the file at `rel` (under the game's `data/`) is changed, the changed
/// copy (made once, in `scratch`). Same length as the original: the engine
/// may size its reads from the game's manifest.
pub(crate) fn game_file(rel: &str, real: &Path, scratch: &Path) -> Option<PathBuf> {
    static MADE: Mutex<Vec<(&str, Option<PathBuf>)>> = Mutex::new(Vec::new());
    let (name, _, patch) = OVERLAYS.iter().find(|(name, applies, _)| rel.eq_ignore_ascii_case(name) && applies())?;
    let mut made = MADE.lock().unwrap();
    if let Some((_, path)) = made.iter().find(|(n, _)| n == name) {
        return path.clone();
    }
    let path = (|| {
        let text = std::fs::read_to_string(real).ok()?;
        let patched = patch(text.clone()).filter(|p| p.len() == text.len())?;
        let path = scratch.join("overlay").join(real.file_name()?);
        std::fs::create_dir_all(path.parent()?).ok()?;
        std::fs::write(&path, patched).ok()?;
        log::info!("overlay: {rel} changed");
        Some(path)
    })();
    made.push((name, path.clone()));
    path
}

/// pausemenu.xml, "Saving And Startup": the engine greys "Startup Flow" out
/// while the landing page preference is on, which later data has an item
/// for; this data has none, so it stays on. With its first item greyed out,
/// the list sends left and right to that item whichever one is highlighted,
/// and "Autosave" could not be turned off. Autosave goes first.
fn autosave_first(text: String) -> Option<String> {
    let line = |pref: &str| {
        let at = text.find(&format!("<MenuPref>{pref}</MenuPref>"))?;
        let start = text[..at].rfind('\n')? + 1;
        Some(start..at + text[at..].find('\n')? + 1)
    };
    let (flow, autosave) = (line("PREF_STARTUP_FLOW")?, line("PREF_AUTOSAVE")?);
    (flow.end == autosave.start)
        .then(|| format!("{}{}{}{}", &text[..flow.start], &text[autosave.clone()], &text[flow], &text[autosave.end..]))
}

/// frontend.xml, the feed (GameStream): the engine stretches its movie to the
/// rectangle given here (exact fit). The movie is drawn for the minimap's
/// width (0.150, the PC's value), but this data gives 0.1989, so the
/// notifications came out a third wider, portraits no longer square. Same
/// length: zeros pad it.
fn feed_width(text: String) -> Option<String> {
    let start = text.find("<GameStream>")?;
    let end = start + text[start..].find("</GameStream>")?;
    let block = &text[start..end];
    let (old, new) = ("sizeX=\"0.19890859375\"", "sizeX=\"0.15000000000\"");
    block.contains(old).then(|| format!("{}{}{}", &text[..start], block.replace(old, new), &text[end..]))
}

#[test]
fn feed_is_the_minimap_width() {
    let text = "<GameStream>\r\n\t<position posX=\"0.0\" sizeX=\"0.19890859375\" />\r\n\t<posloading sizeX=\"0.19890859375\" />\r\n</GameStream>\r\n";
    let want = "<GameStream>\r\n\t<position posX=\"0.0\" sizeX=\"0.15000000000\" />\r\n\t<posloading sizeX=\"0.15000000000\" />\r\n</GameStream>\r\n";
    assert_eq!(feed_width(text.into()).as_deref(), Some(want));
}

#[test]
fn autosave_goes_first() {
    let text = "<a>\r\n\t<Item> <MenuPref>PREF_STARTUP_FLOW</MenuPref> </Item>\r\n\t<Item> <MenuPref>PREF_AUTOSAVE</MenuPref> </Item>\r\n</a>\r\n";
    let want = "<a>\r\n\t<Item> <MenuPref>PREF_AUTOSAVE</MenuPref> </Item>\r\n\t<Item> <MenuPref>PREF_STARTUP_FLOW</MenuPref> </Item>\r\n</a>\r\n";
    assert_eq!(autosave_first(text.into()).as_deref(), Some(want));
}
