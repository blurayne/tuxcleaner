Output docs/tuxcleaner-demo.gif

Require ttyd
Require ffmpeg

Set Shell bash
Set FontFamily "JetBrains Mono"
Set FontSize 18
Set Width 1200
Set Height 720
Set Padding 22
Set Margin 24
Set MarginFill "#d8dee9"
Set WindowBar RingsRight
Set BorderRadius 16
Set Theme { "name": "Adwaita Dark", "black": "#1e1e1e", "red": "#ed333b", "green": "#33d17a", "yellow": "#f6d32d", "blue": "#3584e4", "magenta": "#c061cb", "cyan": "#33c7de", "white": "#deddda", "brightBlack": "#77767b", "brightRed": "#f66151", "brightGreen": "#57e389", "brightYellow": "#f8e45c", "brightBlue": "#62a0ea", "brightMagenta": "#dc8add", "brightCyan": "#5bc8e7", "brightWhite": "#ffffff", "background": "#1e1e1e", "foreground": "#deddda", "selection": "#3584e4", "cursor": "#f6d32d" }
Set TypingSpeed 32ms
Set Framerate 30
Set CursorBlink false

Hide
Type "export HOME='/tmp/tuxcleaner-vhs-demo/home'"
Enter
Type "export XDG_STATE_HOME='/tmp/tuxcleaner-vhs-demo/state'"
Enter
Type "export TUXCLEANER_OS_RELEASE=$PWD/docs/demo/fixtures/os-release"
Enter
Type "export TUXCLEANER_DESKTOP_DIRS=$PWD/docs/demo/fixtures/applications"
Enter
Type "export PATH=$PWD/target/release:$PWD/docs/demo/fixtures/bin"
Enter
Type "export PS1='demo@linux:~$ '"
Enter
Type "cd $HOME"
Enter
Type "/usr/bin/clear"
Enter
Show

Sleep 800ms
Type "tuxcleaner --help"
Sleep 500ms
Enter
Wait+Screen /Options:/
Sleep 2200ms

Ctrl+L
Type "tuxcleaner clean --groups user,dev --dry-run --yes"
Sleep 400ms
Enter
Wait+Screen /Estimated space covered/
Sleep 2200ms

Ctrl+L
Type "tuxcleaner analyze . --min-size 500M"
Sleep 400ms
Enter
Wait+Screen /Large personal files/
Sleep 2400ms

Ctrl+L
Type "tuxcleaner purge --path ~/Projects --older-than-days 30 --dry-run --yes"
Sleep 400ms
Enter
Wait+Screen /Estimated space covered/
Sleep 2200ms

Ctrl+L
Type "tuxcleaner uninstall --search nebula --dry-run"
Sleep 400ms
Enter
Wait+Screen /Nebula Chat/
Sleep 1200ms
Space
Sleep 600ms
Enter
Wait+Screen /Application configuration and user data were preserved/
Sleep 2800ms

Ctrl+L
Type "tuxcleaner history"
Sleep 400ms
Enter
Wait+Screen /succeeded/
Sleep 3500ms
