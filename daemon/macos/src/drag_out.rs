// Mac -> Windows file drag: telling whether a Finder (or any app's) file
// drag is in progress when the cursor leaves for Windows.
//
// Real MWB finds this out on Windows with a hidden helper window placed
// under the cursor to catch the drag's DragEnter. macOS needs no trick:
// every drag writes what's being dragged to the shared drag pasteboard
// (NSPasteboardNameDrag) when it starts. Its contents outlive the drag, so
// a drag counts as current only if the pasteboard changed after the left
// button went down.

use std::path::{Path, PathBuf};

use mwb_protocol::mwb_protocol::MAX_CLIPBOARD_FILE_SIZE;
use objc2::rc::autoreleasepool;
use objc2_app_kit::{NSPasteboard, NSPasteboardNameDrag, NSPasteboardTypeFileURL};
use objc2_foundation::{NSString, NSURL};

pub struct DragOut {
    // The drag pasteboard's changeCount when the left button went down.
    baseline: Option<isize>,
}

impl DragOut {
    pub fn new() -> Self {
        Self { baseline: None }
    }

    /// Call when Windows forwards a left-button press.
    pub fn mouse_down(&mut self) {
        self.baseline = Some(drag_pasteboard_change_count());
    }

    /// The file being dragged right now, if a drag with files on it started
    /// since the last `mouse_down`. Real MWB only ever transfers one file,
    /// so a multi-file drag sends its first.
    pub fn dragged_file(&self) -> Option<PathBuf> {
        let baseline = self.baseline?;
        if drag_pasteboard_change_count() == baseline {
            return None;
        }
        let files = drag_pasteboard_files();
        if files.len() > 1 {
            println!("[drag-out] {} files dragged, only the first goes to Windows (one file at a time, as on real MWB).", files.len());
        }
        files.into_iter().next()
    }
}

impl Default for DragOut {
    fn default() -> Self {
        Self::new()
    }
}

/// Why `path` can't be sent to Windows, if it can't. Real MWB refuses
/// folders ("zip it first") and anything over 100MB.
pub fn unsendable_reason(path: &Path) -> Option<&'static str> {
    match std::fs::metadata(path) {
        Err(_) => Some("it can't be read"),
        Ok(m) if m.is_dir() => Some("folders aren't supported, zip it first"),
        Ok(m) if !m.is_file() => Some("it isn't a regular file"),
        Ok(m) if m.len() as usize > MAX_CLIPBOARD_FILE_SIZE => Some("it's over 100MB"),
        Ok(_) => None,
    }
}

fn drag_pasteboard() -> objc2::rc::Retained<NSPasteboard> {
    NSPasteboard::pasteboardWithName(unsafe { NSPasteboardNameDrag })
}

fn drag_pasteboard_change_count() -> isize {
    autoreleasepool(|_| drag_pasteboard().changeCount())
}

fn drag_pasteboard_files() -> Vec<PathBuf> {
    autoreleasepool(|_| {
        let Some(items) = drag_pasteboard().pasteboardItems() else { return Vec::new() };
        items
            .iter()
            .filter_map(|item| item.stringForType(unsafe { NSPasteboardTypeFileURL }))
            .filter_map(|url| file_url_path(&url))
            .collect()
    })
}

/// A `file://` URL string (percent-encoded, possibly a file reference URL
/// like `file:///.file/id=...`) as a plain path.
fn file_url_path(url: &NSString) -> Option<PathBuf> {
    let url = NSURL::URLWithString(url)?;
    // Resolves file reference URLs to a path URL; a path URL comes back as is.
    let url = url.filePathURL().unwrap_or(url);
    url.path().map(|p| PathBuf::from(p.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regular_files_are_sendable_and_folders_are_not() {
        let dir = std::env::temp_dir().join(format!("mwb-drag-out-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("note.txt");
        std::fs::write(&file, b"hi").unwrap();

        assert_eq!(unsendable_reason(&file), None);
        assert_eq!(unsendable_reason(&dir), Some("folders aren't supported, zip it first"));
        assert_eq!(unsendable_reason(&dir.join("missing")), Some("it can't be read"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn file_urls_decode_to_paths() {
        let url = NSString::from_str("file:///Users/someone/My%20File.txt");
        assert_eq!(file_url_path(&url), Some(PathBuf::from("/Users/someone/My File.txt")));
    }
}
