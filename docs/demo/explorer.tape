# The explorer demo in the README and on the website.
# Regenerate from the repository root (needs vhs and detangle on PATH):
#   bash docs/demo/setup.sh && vhs docs/demo/explorer.tape
Output docs/demo/explorer.gif
Require detangle

Set Shell "bash"
Set FontSize 14
Set Width 1280
Set Height 740
Set Padding 14
Set TypingSpeed 55ms

Hide
Type "cd /tmp/detangle-demo && export PS1='$ ' && clear"
Enter
# Start recording once the explorer is on screen.
Type "detangle"
Enter
Sleep 1.5s
Show
Sleep 2s

# Find a module, then look at what it imports.
Type "/"
Sleep 300ms
Type "components/App.tsx"
Sleep 700ms
Enter
Sleep 1s
Right
Sleep 1.2s
Down 4
Sleep 1s
Screenshot docs/demo/explorer.png
# Follow the selected import.
Enter
Sleep 1.8s

# Cycles, violations and hotspots.
Type "3"
Sleep 2.8s
Type "2"
Sleep 2.2s
Type "4"
Sleep 2.2s
Type "q"
Sleep 500ms
