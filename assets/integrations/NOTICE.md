# Herdr integration attribution (kathamo-status)

`opencode-tui.js` is vendored unmodified from `herdrdev/herdr`,
`src/integration/assets/opencode/herdr-tui-session.js`, revision
`bc8c28988f75c538d4ca058b702f5adbea30566e` (retrieved 2026-10-29).
It is distributed under the Apache License 2.0; see `HERDR-LICENSE`.

Source: https://github.com/herdrdev/herdr/blob/bc8c28988f75c538d4ca058b702f5adbea30566e/src/integration/assets/opencode/herdr-tui-session.js

Kathamo implements only the scoped native reporting portion of Herdr's protocol.
It does not implement Herdr's general control, metadata, or session restoration API.

`tests/integrations/opencode-tui.test.ts` is from the same revision; only its
import path and attribution header are adapted for this repository.
