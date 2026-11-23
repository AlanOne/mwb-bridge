using System.Runtime.InteropServices;

namespace MwbBridgeCompanion;

static class Native
{
    const int VK_LBUTTON = 0x01;
    const uint INPUT_MOUSE = 0;
    const uint MOUSEEVENTF_LEFTDOWN = 0x0002;
    const uint MOUSEEVENTF_LEFTUP = 0x0004;

    public const int WS_EX_TOPMOST = 0x00000008;
    public const int WS_EX_TOOLWINDOW = 0x00000080;
    public const int WS_EX_NOACTIVATE = 0x08000000;
    static readonly IntPtr HWND_TOPMOST = new(-1);
    const uint SWP_NOACTIVATE = 0x0010;
    const uint SWP_SHOWWINDOW = 0x0040;

    [StructLayout(LayoutKind.Sequential)]
    struct MOUSEINPUT
    {
        public int dx;
        public int dy;
        public uint mouseData;
        public uint dwFlags;
        public uint time;
        public IntPtr dwExtraInfo;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct KEYBDINPUT
    {
        public ushort wVk;
        public ushort wScan;
        public uint dwFlags;
        public uint time;
        public IntPtr dwExtraInfo;
    }

    [StructLayout(LayoutKind.Explicit)]
    struct InputUnion
    {
        [FieldOffset(0)] public MOUSEINPUT mi;
        [FieldOffset(0)] public KEYBDINPUT ki;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct INPUT
    {
        public uint type;
        public InputUnion u;
    }

    [DllImport("user32.dll")]
    static extern short GetAsyncKeyState(int vKey);

    [DllImport("user32.dll", SetLastError = true)]
    static extern uint SendInput(uint nInputs, INPUT[] pInputs, int cbSize);

    [DllImport("user32.dll", SetLastError = true)]
    static extern bool SetWindowPos(IntPtr hWnd, IntPtr hWndInsertAfter, int x, int y, int cx, int cy, uint flags);

    [StructLayout(LayoutKind.Sequential)]
    struct POINT { public int X; public int Y; }

    [DllImport("user32.dll")]
    static extern IntPtr WindowFromPoint(POINT p);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    static extern int GetClassName(IntPtr hWnd, System.Text.StringBuilder name, int max);

    [DllImport("user32.dll")]
    static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);

    /// The window under a screen point, described for the log.
    public static string DescribeWindowAt(Point p)
    {
        var hwnd = WindowFromPoint(new POINT { X = p.X, Y = p.Y });
        var name = new System.Text.StringBuilder(128);
        GetClassName(hwnd, name, name.Capacity);
        GetWindowThreadProcessId(hwnd, out var pid);
        string process;
        try { process = System.Diagnostics.Process.GetProcessById((int)pid).ProcessName; } catch { process = "?"; }
        return $"0x{hwnd.ToInt64():x} {name} ({process})";
    }

    /// Whether Windows thinks the left button is down.
    public static bool LeftButtonDown() => (GetAsyncKeyState(VK_LBUTTON) & 0x8000) != 0;

    /// Posts a left button press or release at the cursor's position.
    public static void LeftButton(bool down)
    {
        var input = new INPUT { type = INPUT_MOUSE };
        input.u.mi.dwFlags = down ? MOUSEEVENTF_LEFTDOWN : MOUSEEVENTF_LEFTUP;
        if (SendInput(1, new[] { input }, Marshal.SizeOf<INPUT>()) != 1)
        {
            Log.Write($"SendInput failed: {Marshal.GetLastWin32Error()}");
        }
    }

    /// Puts a window above every other topmost window, without activating it.
    public static void RaiseTopmost(IntPtr hwnd, Rectangle bounds)
    {
        SetWindowPos(hwnd, HWND_TOPMOST, bounds.X, bounds.Y, bounds.Width, bounds.Height, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    }
}
