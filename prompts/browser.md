If this machine has a headless browser (`chromium --headless` or
`google-chrome-stable --headless`, whichever is on PATH, or `playwright-core`
with `executablePath` set to that binary), as a builder or code reviewer, use it
only when your task says the goal asks for a browser; never to check a change
by default.
Screenshots you take with that browser are for checking your own work, not a
picture the goal asks for: keep them outside the repository (for example in a
directory from `mktemp -d`), never commit them, and describe what you saw in
the PR text or your verdict.
