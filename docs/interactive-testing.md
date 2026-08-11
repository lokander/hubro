# Interactive testing (screenshots + clicking)

How to drive the app to verify a UI change. Development happens on multiple machines; pick the recipe for the current platform.

The app writes to the *real* `$XDG_CONFIG_HOME/hubro/`, so launch it with `XDG_CONFIG_HOME`/`XDG_DATA_HOME` pointed at a scratch dir rather than cleaning up afterwards, and `stat` them before and after to confirm. The scratch dirs don't cover passwords: saving a connection writes to the real `hubro` keyring service, so also set `HUBRO_DISABLE_KEYRING=1` (forces the session-only fallback, `src/secrets.rs`). Parallel agents each need their own display (`Xephyr :12`, `:13`, …), and must kill the app and the display **by recorded PID** — every worktree builds a binary called `hubro`, so `pkill -x hubro` takes the siblings' down too.

## Linux (KDE Plasma on Wayland)

To drive the app (click, type, screenshot) without touching the user's real cursor, run it inside a nested Xephyr X server:

```bash
Xephyr :2 -screen 900x700 &
DISPLAY=:2 GDK_BACKEND=x11 ./target/dx/hubro/debug/linux/app/hubro &   # binary path from `dx build`
DISPLAY=:2 xdotool search --name "Hubro" windowmove 50 50                  # window is named "Hubro"; may spawn offscreen
DISPLAY=:2 xdotool mousemove X Y click 1                                     # full pointer control
DISPLAY=:2 xdotool key --clearmodifiers s e l e c t space 1                  # keyboard: keysyms, not `type` — see gotchas
import -display :2 -window root shot.png                                     # screenshot the nested display
```

Driving the app directly on the desktop half-works and isn't worth it: `spectacle -b -n -a -o shot.png` captures windows and `xdotool key` reaches XWayland windows (after a one-time KDE "Remote Control" approval), but KWin ignores XTEST pointer events, so mouse control is impossible outside Xephyr.

Gotchas: **`xdotool type` silently drops keystrokes** into the webview — seven characters arrive as five, or as none at all. Send keysyms instead (`xdotool key --clearmodifiers a b c`), which is reliable and still fast enough to land inside a 250 ms debounce window. Screenshot the field and count the characters before submitting anything that matters. Native `<select>` dropdowns are driven by click → `key Down/Up` → `key Return` (options aren't clickable elements). Xephyr runs no window manager, so nothing delivers `WindowEvent::CloseRequested` — send a synthetic `WM_DELETE_WINDOW` ClientMessage (a ~30-line Xlib/`gcc -lX11` helper) to test the window-close guard.

**"Nothing happened" is not a measurement unless the app ran.** A negative case once reported exactly the expected zero because Xephyr had never come up. Assert the display (`xdpyinfo`) and the window (`xdotool search`) before believing a null result, and show the run reached the state under test — a screenshot of the *next* screen is what makes "it didn't fire" mean something.

**Build a negative control.** Nothing in the suite covers rendering, so a passing interactive check does not show the script exercises the bug. Rebuild with only the fix reverted and re-run the identical script: if it still passes, the script is testing nothing. That turned a "verified" FRE-154 run — which would have passed either way — into a real before/after.

## macOS

The app is a native Cocoa bundle — there is no display server to nest, so **synthetic input drives the real cursor** (no Xephyr-style isolation exists). Keep interactions short: screenshot → verify → act, and save/restore the pointer around clicks. Tools: `brew install cliclick smokris/getwindowid/getwindowid`. One-time grants for the terminal app in System Settings → Privacy & Security: **Accessibility** (cliclick/System Events) and **Screen Recording** (screencapture).

```bash
dx build    # bundle: target/dx/hubro/debug/macos/Hubro.app
open target/dx/hubro/debug/macos/Hubro.app    # must go via LaunchServices — see the blank-webview gotcha; quit with pkill -x Hubro
GetWindowID Hubro --list     # titles list as "(null)" — pick the id with the main window's size
screencapture -x -l <id> shot.png                                         # crisp per-window capture, works unfocused
osascript -e 'tell app "System Events" to tell (first process whose unix id is '$(pgrep -x Hubro)') to get {position, size} of window 1'
POS=$(cliclick p | tr -d ' '); cliclick c:X,Y; cliclick "m:$POS"          # click, then restore the cursor
```

Gotchas: on current macOS the webview stays **blank when the binary is exec'd directly** from a terminal (the window opens but WKWebView never paints) — launch through LaunchServices instead (`open path/to/Hubro.app`, then `pkill -x hubro` to quit; note the release bundle's process name is lowercase `hubro`, the debug bundle's is `Hubro`). Click targets are **window position + logical (point) coordinates** from the osascript line — don't derive them from screenshot pixels, which are 2x Retina and include shadow margins. The first click on an unfocused window only focuses it (the webview doesn't accept click-through) — click twice or activate the app first. Keystrokes go via System Events (`keystroke`/`key code`) to the focused window. The window-close guard is testable directly: macOS has a real window manager, so the red button or Cmd+W delivers `CloseRequested` — no synthetic-event helper needed.

## Windows

Everything goes through **posted window messages** (`PostMessage` to the WebView2 child) — no real cursor movement, no focus stealing, and no permission grants needed. Dot-source the checked-in helper `scripts/winauto.ps1` (PowerShell 7) for all of it:

```powershell
dx build    # exe: target\dx\hubro\debug\windows\app\hubro.exe
Start-Process .\target\dx\hubro\debug\windows\app\hubro.exe   # window title "Hubro"
. .\scripts\winauto.ps1
$h = Find-AppWindow                 # top-level HWND (throws if the app isn't running)
Save-WindowShot $h shot.png         # crisp PrintWindow capture, works while occluded; 1:1 with click coords
Send-PostedClick $h X Y             # coords relative to the window rect = the screenshot's pixel coords
Send-PostedText  $h "localhost"     # WM_CHAR into the focused element — click a field first
Send-PostedKey   $h 0x28            # virtual keys: 0x0D Enter, 0x1B Esc, 0x26/0x28 Up/Down, 0x09 Tab
Send-PostedWheel $h X Y -3          # scroll down 3 notches at that point
Send-Close       $h                 # WM_CLOSE → delivers CloseRequested (tests the close guard)
```

Build prerequisites beyond rustup + VS Build Tools: **NASM** (`aws-lc-sys` needs it; GitHub's windows-latest runners have it preinstalled, dev machines don't — drop `nasm.exe` from nasm.us into `~\.cargo\bin`) and the WebView2 runtime (preinstalled on Win11). Install dx from the prebuilt `dx-x86_64-pc-windows-msvc.zip` GitHub release asset rather than `cargo install`.

Gotchas: native `<select>` dropdowns work like on Linux — posted click, then `Send-PostedKey` Down/Up + Enter (the popup is a separate OS window that won't appear in captures; drive it blind by keyboard). **Re-screenshot before deriving coordinates** — form layouts shift as content changes and a click 5px off a field silently does nothing. Screenshots include the title bar and native menu (webview content starts ~56px down). Coordinates are 1:1 only at 100% display scaling (the helper calls `SetProcessDPIAware`; both dev monitors are at 100%). Posted input is verified with the app foreground (its normal state after launch); it never disturbs other windows either way. In the helper, Win32 `FindWindowEx` needs `[NullString]::Value` — a PowerShell `$null` string marshals as `""` and matches nothing.
