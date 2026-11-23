// File drag-and-drop between machines, the part of real MWB's
// `DragDrop.cs` state machine a controlled (non-Windows) machine plays.
// OS-agnostic: it only decides *what* to do from the packages and mouse
// messages Windows sends; the platform build does the pulling, pushing and
// pasteboard reading. See PROTOCOL.md's drag-and-drop section for the full
// sequence as Windows runs it.

use crate::input_handling::{WM_LBUTTONUP, WM_RBUTTONUP};
use crate::mwb_protocol::{
    ID_ALL, PACKAGE_TYPE_CLIPBOARD_DRAG_DROP, PACKAGE_TYPE_CLIPBOARD_DRAG_DROP_END,
    PACKAGE_TYPE_CLIPBOARD_DRAG_DROP_OPERATION, PACKAGE_TYPE_EXPLORER_DRAG_DROP,
};

/// What the platform build should do after a package or mouse message.
#[derive(Debug, PartialEq, Eq)]
pub enum DragAction {
    None,
    /// A file dragged on Windows was released over this machine: pull it
    /// (real MWB's `DragDropStep10`, `GetRemoteClipboard("desktop")`).
    PullDrop,
    /// Windows saw the mouse leave this machine with the left button held
    /// and asks whether that's a file being dragged out (`ExplorerDragDrop`,
    /// `DragDropStep03`). If it is, offer it to machine `to`.
    CheckOutgoingDrag { to: u32 },
}

/// Drop-side state: whether a file dragged on Windows is currently over
/// this machine, waiting for the mouse button to be released.
pub struct DragDropState {
    my_id: u32,
    dropping: bool,
}

impl DragDropState {
    pub fn new(my_id: u32) -> Self {
        Self { my_id, dropping: false }
    }

    pub fn is_dropping(&self) -> bool {
        self.dropping
    }

    /// Handles a package from Windows. `src`/`des` are its Src/Des fields.
    pub fn on_package(&mut self, package_type: u8, src: u32, des: u32) -> DragAction {
        match package_type {
            // Broadcast "some machine has a file being dragged"; who has it
            // is the connection it came over, so nothing to remember here.
            PACKAGE_TYPE_CLIPBOARD_DRAG_DROP => DragAction::None,
            // "You're the drop target" (DragDropStep08_2).
            PACKAGE_TYPE_CLIPBOARD_DRAG_DROP_OPERATION => {
                if des == self.my_id {
                    self.dropping = true;
                }
                DragAction::None
            }
            // The drag went back without a drop (DragDropStep12).
            PACKAGE_TYPE_CLIPBOARD_DRAG_DROP_END => {
                self.dropping = false;
                DragAction::None
            }
            PACKAGE_TYPE_EXPLORER_DRAG_DROP if des == self.my_id || des == ID_ALL => {
                DragAction::CheckOutgoingDrag { to: src }
            }
            _ => DragAction::None,
        }
    }

    /// Handles a mouse message (a Mouse package's dwFlags) forwarded from
    /// Windows (DragDropStep01/09).
    pub fn on_mouse(&mut self, flags: u32) -> DragAction {
        match flags {
            WM_LBUTTONUP if self.dropping => {
                self.dropping = false;
                DragAction::PullDrop
            }
            // A right click while dropping cancels it, as on real MWB.
            WM_RBUTTONUP => {
                self.dropping = false;
                DragAction::None
            }
            _ => DragAction::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input_handling::{WM_LBUTTONDOWN, WM_MOUSEMOVE};

    const ME: u32 = 2;
    const WINDOWS: u32 = 7;

    fn dropping() -> DragDropState {
        let mut s = DragDropState::new(ME);
        assert_eq!(s.on_package(PACKAGE_TYPE_CLIPBOARD_DRAG_DROP, WINDOWS, ID_ALL), DragAction::None);
        assert_eq!(s.on_package(PACKAGE_TYPE_CLIPBOARD_DRAG_DROP_OPERATION, WINDOWS, ME), DragAction::None);
        s
    }

    #[test]
    fn releasing_the_left_button_over_us_pulls_the_dropped_file() {
        let mut s = dropping();
        assert_eq!(s.on_mouse(WM_MOUSEMOVE), DragAction::None);
        assert_eq!(s.on_mouse(WM_LBUTTONUP), DragAction::PullDrop);
        // Only once: the next release is an ordinary click.
        assert_eq!(s.on_mouse(WM_LBUTTONDOWN), DragAction::None);
        assert_eq!(s.on_mouse(WM_LBUTTONUP), DragAction::None);
    }

    #[test]
    fn a_plain_click_without_a_drag_pulls_nothing() {
        let mut s = DragDropState::new(ME);
        assert_eq!(s.on_mouse(WM_LBUTTONUP), DragAction::None);
    }

    #[test]
    fn a_drop_operation_for_another_machine_is_ignored() {
        let mut s = DragDropState::new(ME);
        s.on_package(PACKAGE_TYPE_CLIPBOARD_DRAG_DROP_OPERATION, WINDOWS, 9);
        assert_eq!(s.on_mouse(WM_LBUTTONUP), DragAction::None);
    }

    #[test]
    fn the_drag_going_back_cancels_the_drop() {
        let mut s = dropping();
        s.on_package(PACKAGE_TYPE_CLIPBOARD_DRAG_DROP_END, WINDOWS, ME);
        assert_eq!(s.on_mouse(WM_LBUTTONUP), DragAction::None);
    }

    #[test]
    fn a_right_click_cancels_the_drop() {
        let mut s = dropping();
        s.on_mouse(WM_RBUTTONUP);
        assert_eq!(s.on_mouse(WM_LBUTTONUP), DragAction::None);
    }

    #[test]
    fn explorer_drag_drop_asks_us_to_check_for_an_outgoing_drag() {
        let mut s = DragDropState::new(ME);
        assert_eq!(
            s.on_package(PACKAGE_TYPE_EXPLORER_DRAG_DROP, WINDOWS, ME),
            DragAction::CheckOutgoingDrag { to: WINDOWS }
        );
        assert_eq!(s.on_package(PACKAGE_TYPE_EXPLORER_DRAG_DROP, WINDOWS, 9), DragAction::None);
    }
}
