# Reference video → implemented features

Source: the supplied 26 s portrait screen recording (720×988), reviewed frame by frame at 1 fps
before the dashboard was built. The video shows a desk branded by its original author. The
Pandarianz version copies the **functional layout and interaction rhythm only**, uses the supplied
Pandarianz panda art, and omits the author's logos, venue badges and referral links.

| Observed in the video | Implemented as | Notes / assumptions |
|---|---|---|
| Header: logo + desk name, tagline, day/cycle text, boxed UTC clock top-right | Header with supplied panda art, "PANDABRAIN DESK", tagline, SHADOW·PAPER badge, DEMO badge, engine badge, live UTC clock | Mode badges added (required); always visible |
| 4×2 grid of seat cards (CHIEF, SCAN, VET, SOCIAL / SIZE, FILLS, RISK, CRAWLER): coloured name, small subtitle, a metric with a coloured bar, a quoted status line | Same 4×2 grid. 8th card is **WATCHLIST** (the crawler analogue: durable watchlist). Each card shows a real state pill, a real metric/bar, the seat's last action as a quote, and a timestamp | Bars show real ratios (e.g. passed/seen, ticket vs max). No invented scores |
| Large desk floor: desks with monitors, coloured blob avatars that move between desks; network-line bursts between seats; speech-bubble status labels; big wall screen with a candle chart; side boards; rocket/radar props | SVG desk floor: six desks + CHIEF station, panda avatars per seat that move to their monitor while running and glow by state (running / holding / blocked / error / unconfigured); **handoff lines with node bursts drawn when consecutive backend events come from different seats**; speech bubbles = last real action; wall screen shows the open position's **recorded marks** or the last CHIEF outcome; board shows the last cycle; WATCH RADAR sweeps while scanning; BOOK vault shows free/held/close-pending | The video's candle chart is replaced by a line of real recorded marks (we hold no OHLC history and do not fabricate any) |
| Ticker line over the floor (balance, PnL, tickets, holders, day) | Ticker: cash, equity, closed trades, net PnL, open token, cycle # | Values from the book |
| "BALANCE" panel: green equity line, big balance figure, red/green volume-style bars underneath | BALANCE · PAPER: equity line from recorded equity points, big equity figure, bars = candidates seen per cycle (green = reached the judge) | Empty state until real cycles exist |
| Bottom-left "ANALYSIS": token chart + metric list | PANDABRAIN · ANALYSIS: engine, score type, provenance (model ID, threshold + settings versions), pick/no-trade reason, ranked candidates, open position with mark chart and exit levels | |
| Bottom-right `thresholds.py`: HARD and SOFT rows with live values and ✓, plus stage pills (free_kill, trade_kill, chain_kill, soft_kill) | `thresholds.py` panel: HARD (pre-dossier), VET facts, VALUATION (labelled by type), JUDGE rows with the focused candidate's real values and ✓/✗; funnel pills with counts and rejection-reason pills from the last cycle | |
| Motion: steady, subtle pulses; bursts on handoff | CSS pulses/bob for running seats, dashed handoff animation, radar sweep. **All motion is disabled under `prefers-reduced-motion`** | |
| Hot-pink frame / palette | Dark palette with bamboo-green accents and the violet glow of the supplied panda art | Original Pandarianz styling, not the video's |

Assumptions where the video was unclear:
- The pink avatars' exact paths are not reproducible from 1 fps frames. Avatars move between a
  "home" spot beside their desk and their monitor, and handoffs are drawn as curved links.
- The video's per-seat numbers (e.g. 0.89, 0.20) look like model outputs. Here they are replaced by
  real desk metrics, so no probability is shown unless a real TYPED_AI judgment produced it.
