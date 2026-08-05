# winauto.ps1 — drive the dataview app on Windows for interactive testing:
# per-window screenshots and synthetic input via posted window messages.
# Nothing here moves the real cursor or types into other windows (except the
# explicit Send-RealClick fallback). Dot-source it: . .\scripts\winauto.ps1
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class WinAuto {
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr lParam);
    public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr hWnd, StringBuilder text, int count);
    [DllImport("user32.dll")] public static extern int GetClassName(IntPtr hWnd, StringBuilder text, int count);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hWnd, IntPtr hdc, uint flags);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr hWnd, uint msg, IntPtr wParam, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);
    [DllImport("user32.dll")] public static extern void mouse_event(uint flags, int dx, int dy, uint data, IntPtr extra);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindowEx(IntPtr parent, IntPtr after, string cls, string title);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
}
"@
[WinAuto]::SetProcessDPIAware() | Out-Null

# First visible, titled top-level window owned by the process (the app window is titled "dataview").
function Find-AppWindow([string]$ProcName = 'dataview') {
    $procs = @(Get-Process $ProcName -ErrorAction SilentlyContinue)
    if (-not $procs) { throw "no process '$ProcName'" }
    $script:found = [IntPtr]::Zero
    $cb = [WinAuto+EnumWindowsProc]{
        param($h, $l)
        $pid2 = 0u
        [WinAuto]::GetWindowThreadProcessId($h, [ref]$pid2) | Out-Null
        if (($procs.Id -contains [int]$pid2) -and [WinAuto]::IsWindowVisible($h)) {
            $sb = New-Object System.Text.StringBuilder 256
            [WinAuto]::GetWindowText($h, $sb, 256) | Out-Null
            if ($sb.Length -gt 0) { $script:found = $h; return $false }
        }
        return $true
    }
    [WinAuto]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
    if ($script:found -eq [IntPtr]::Zero) { throw "no visible titled top-level window for '$ProcName'" }
    $script:found
}

function Get-WindowInfo([IntPtr]$hWnd) {
    $r = New-Object WinAuto+RECT
    [WinAuto]::GetWindowRect($hWnd, [ref]$r) | Out-Null
    $sb = New-Object System.Text.StringBuilder 256
    [WinAuto]::GetWindowText($hWnd, $sb, 256) | Out-Null
    [pscustomobject]@{ HWnd = $hWnd; Title = $sb.ToString(); Left = $r.Left; Top = $r.Top; Width = $r.Right - $r.Left; Height = $r.Bottom - $r.Top }
}

# Crisp per-window capture (PW_RENDERFULLCONTENT); works while occluded, image is 1:1
# with window-rect coordinates at 100% display scaling.
function Save-WindowShot([IntPtr]$hWnd, [string]$Path) {
    $r = New-Object WinAuto+RECT
    [WinAuto]::GetWindowRect($hWnd, [ref]$r) | Out-Null
    $bmp = New-Object System.Drawing.Bitmap ($r.Right - $r.Left), ($r.Bottom - $r.Top)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $hdc = $g.GetHdc()
    $ok = [WinAuto]::PrintWindow($hWnd, $hdc, 2)
    $g.ReleaseHdc($hdc); $g.Dispose()
    $bmp.Save($Path, [System.Drawing.Imaging.ImageFormat]::Png); $bmp.Dispose()
    if (-not $ok) { Write-Warning "PrintWindow returned false" }
    $Path
}

# Child chain under the app window: WRY_WEBVIEW -> Chrome_WidgetWin_0 -> _1 -> Chrome_RenderWidgetHostHWND.
# Input goes to the innermost (render widget) window.
function Get-WebViewChild([IntPtr]$hWnd) {
    $target = $hWnd
    $chain = @()
    while ($true) {
        # NB: [NullString]::Value, not $null — PowerShell marshals $null as "" and FindWindowEx finds nothing
        $c = [WinAuto]::FindWindowEx($target, [IntPtr]::Zero, [NullString]::Value, [NullString]::Value)
        if ($c -eq [IntPtr]::Zero) { break }
        $sb = New-Object System.Text.StringBuilder 256
        [WinAuto]::GetClassName($c, $sb, 256) | Out-Null
        $r = New-Object WinAuto+RECT
        [WinAuto]::GetWindowRect($c, [ref]$r) | Out-Null
        $chain += [pscustomobject]@{ HWnd = $c; Class = $sb.ToString(); Left = $r.Left; Top = $r.Top; Width = $r.Right - $r.Left; Height = $r.Bottom - $r.Top }
        $target = $c
    }
    $chain
}

# Click at coords relative to the top-level window rect (same space as Save-WindowShot images).
function Send-PostedClick([IntPtr]$hWnd, [int]$X, [int]$Y) {
    $wr = New-Object WinAuto+RECT
    [WinAuto]::GetWindowRect($hWnd, [ref]$wr) | Out-Null
    $chain = @(Get-WebViewChild $hWnd)
    if (-not $chain) { throw "no WebView2 child windows found" }
    $inner = $chain[-1]
    $cx = $wr.Left + $X - $inner.Left; $cy = $wr.Top + $Y - $inner.Top
    $lp = [IntPtr](($cy -shl 16) -bor ($cx -band 0xFFFF))
    [WinAuto]::PostMessage($inner.HWnd, 0x0200, [IntPtr]::Zero, $lp) | Out-Null  # WM_MOUSEMOVE
    Start-Sleep -Milliseconds 30
    [WinAuto]::PostMessage($inner.HWnd, 0x0201, [IntPtr]1, $lp) | Out-Null       # WM_LBUTTONDOWN
    Start-Sleep -Milliseconds 60
    [WinAuto]::PostMessage($inner.HWnd, 0x0202, [IntPtr]0, $lp) | Out-Null       # WM_LBUTTONUP
}

# Type text into the focused element (posted-click a field first to place the caret).
function Send-PostedText([IntPtr]$hWnd, [string]$Text) {
    $inner = @(Get-WebViewChild $hWnd)[-1]
    foreach ($ch in $Text.ToCharArray()) {
        [WinAuto]::PostMessage($inner.HWnd, 0x0102, [IntPtr][int]$ch, [IntPtr]1) | Out-Null  # WM_CHAR
        Start-Sleep -Milliseconds 15
    }
}

# Press a virtual key: 0x0D Enter, 0x09 Tab, 0x1B Esc, 0x26 Up, 0x28 Down, 0x25 Left, 0x27 Right.
function Send-PostedKey([IntPtr]$hWnd, [int]$VirtualKey) {
    $inner = @(Get-WebViewChild $hWnd)[-1]
    [WinAuto]::PostMessage($inner.HWnd, 0x0100, [IntPtr]$VirtualKey, [IntPtr]1) | Out-Null  # WM_KEYDOWN
    Start-Sleep -Milliseconds 30
    [WinAuto]::PostMessage($inner.HWnd, 0x0101, [IntPtr]$VirtualKey, [IntPtr](0xC0000001 -band 0xFFFFFFFF)) | Out-Null  # WM_KEYUP
}

# Scroll at window-relative coords; $Notches positive = up, negative = down.
function Send-PostedWheel([IntPtr]$hWnd, [int]$X, [int]$Y, [int]$Notches) {
    $wr = New-Object WinAuto+RECT
    [WinAuto]::GetWindowRect($hWnd, [ref]$wr) | Out-Null
    $sx = $wr.Left + $X; $sy = $wr.Top + $Y
    $inner = @(Get-WebViewChild $hWnd)[-1]
    $wp = [IntPtr](((120 * $Notches) -band 0xFFFF) -shl 16)
    $lp = [IntPtr](($sy -shl 16) -bor ($sx -band 0xFFFF))   # NB: wheel lParam is SCREEN coords
    [WinAuto]::PostMessage($inner.HWnd, 0x020A, $wp, $lp) | Out-Null
}

# Deliver WindowEvent::CloseRequested (exercises the unsaved-edits close guard).
function Send-Close([IntPtr]$hWnd) {
    [WinAuto]::PostMessage($hWnd, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null  # WM_CLOSE
}

# Fallback only — moves the real cursor (saved and restored around the click).
# Use when posted input doesn't reach some control; prefer Send-PostedClick.
function Send-RealClick([IntPtr]$hWnd, [int]$X, [int]$Y) {
    $r = New-Object WinAuto+RECT
    [WinAuto]::GetWindowRect($hWnd, [ref]$r) | Out-Null
    $save = New-Object WinAuto+POINT
    [WinAuto]::GetCursorPos([ref]$save) | Out-Null
    [WinAuto]::SetForegroundWindow($hWnd) | Out-Null
    Start-Sleep -Milliseconds 100
    [WinAuto]::SetCursorPos($r.Left + $X, $r.Top + $Y) | Out-Null
    Start-Sleep -Milliseconds 40
    [WinAuto]::mouse_event(2, 0, 0, 0, [IntPtr]::Zero)   # LEFTDOWN
    [WinAuto]::mouse_event(4, 0, 0, 0, [IntPtr]::Zero)   # LEFTUP
    Start-Sleep -Milliseconds 40
    [WinAuto]::SetCursorPos($save.X, $save.Y) | Out-Null
}
