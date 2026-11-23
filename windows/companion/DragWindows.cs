namespace MwbBridgeCompanion;

/// A nearly invisible, never-activating topmost window. Not fully
/// transparent: a window Windows sees as transparent lets the mouse and
/// drags fall through to whatever is below.
class GhostWindow : Form
{
    public GhostWindow()
    {
        FormBorderStyle = FormBorderStyle.None;
        ShowInTaskbar = false;
        StartPosition = FormStartPosition.Manual;
        TopMost = true;
        BackColor = Color.Black;
        Opacity = 0.01;
    }

    protected override bool ShowWithoutActivation => true;

    protected override CreateParams CreateParams
    {
        get
        {
            var cp = base.CreateParams;
            cp.ExStyle |= Native.WS_EX_TOOLWINDOW | Native.WS_EX_NOACTIVATE | Native.WS_EX_TOPMOST;
            return cp;
        }
    }

    public void ShowOver(Rectangle bounds)
    {
        Bounds = bounds;
        if (!Visible) Show();
        Native.RaiseTopmost(Handle, bounds);
    }
}

/// Windows -> Mac: covers the whole desktop while a drag that left Windows
/// is over the Mac. Mouse Without Borders parks the Windows cursor while
/// another machine has control, and the Windows drag goes on hovering
/// there, so this window, being under the parked cursor, receives it: that
/// is how the dragged files are learned (all of them, unlike PowerToys,
/// which only reports single-file drags). It also gives that leftover drag
/// somewhere harmless to end.
sealed class CatcherWindow : GhostWindow
{
    public event Action<string[]>? FilesCaught;
    public event Action<int>? DragEntered;
    public event Action? Dropped;

    public CatcherWindow()
    {
        AllowDrop = true;
        DragEnter += (_, e) =>
        {
            DragEntered?.Invoke(e.KeyState);
            if (e.Data?.GetData(DataFormats.FileDrop) is string[] files && files.Length > 0)
            {
                e.Effect = DragDropEffects.Copy;
                FilesCaught?.Invoke(files);
            }
            else
            {
                e.Effect = DragDropEffects.None;
            }
        };
        DragOver += (_, e) => e.Effect = e.Data?.GetDataPresent(DataFormats.FileDrop) == true ? DragDropEffects.Copy : DragDropEffects.None;
        // The drop that ends the leftover drag: the files already went to the Mac.
        DragDrop += (_, _) => Dropped?.Invoke();
    }
}

/// Mac -> Windows: a small window put under the cursor that a simulated
/// left press lands on, which it turns into a real Windows drag of the
/// files (the same trick the Mac side uses, and PowerToys' own helper).
sealed class SourceWindow : GhostWindow
{
    /// Raised on the press; the handler runs the (modal) drag.
    public event Action? Pressed;

    public SourceWindow()
    {
        MouseDown += (_, e) =>
        {
            if (e.Button == MouseButtons.Left) Pressed?.Invoke();
        };
    }
}
