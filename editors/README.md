# Syntax highlighting for watershed build files

A TextMate grammar for `.ws` build files, laid out as a VS Code extension so
the same folder works in both editors.

## IntelliJ IDEA (and other JetBrains IDEs, Community included)

1. *Settings → Editor → TextMate Bundles → +*
2. Choose this folder (`editors/watershed-build`) and press OK.
3. Open any `.ws` file.

If `.ws` files still open as plain text, remove `*.ws` from *Settings → Editor
→ File Types → Text* (or whichever type claimed it).

## VS Code

Copy or link this folder into your extensions folder and restart VS Code:

```bash
ln -s "$PWD/editors/watershed-build" ~/.vscode/extensions/watershed-build
```

## What it highlights

Keywords (`module`, `val`, `defaults`, `if`/`else`), the settings a module can
have, `dependsOn` and `dependency`, the built-in functions (`env`, `min`,
`max`, `abs`), numbers, strings and their escapes, comments, and the group,
artifact and version inside a Maven coordinate.

This is colouring only. For errors as you type, completion, and hovering to see
a value and where it came from, connect `wsb lsp`, the language server. In
IntelliJ IDEA, install the LSP4IJ plugin, then add a new language server with
the command `wsb lsp` and map it to `*.ws` files. VS Code needs a small client
extension, which does not exist yet.
