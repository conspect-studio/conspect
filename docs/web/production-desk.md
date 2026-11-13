# Production desk

The desktop desk uses the viewport as its working area at widths of 1280 px or more and heights of 700 px or more. Program and Preview share the top row. The source wall occupies the next row; twelve sources use six columns, and thirteen through twenty-four use eight. Two compact bus banks and CUT/AUTO remain beneath the wall. Smaller or zoomed viewports retain document flow rather than clipping controls.

Click or keyboard-activate a source tile to select Preview. This uses the same plan, source eligibility, freshness and in-flight guard as the Preview bus. Direct Program taps still confirm. Locked CUT/AUTO still confirm once. Tally remains engine-confirmed and independent from signal state.

Source names wrap in the wall and bus keys. The wall retains transport/still notes. Timecode and show-clock absence remain explicit in a compact header strip. The Tally & engine status disclosure retains alarm details, monitoring, layout counts and unsupported-control explanations. AUTO frames remain directly editable. Route / patch opens the existing routing page; it does not apply a route.

Each monitor caption reserves `.bd-monitor-actions[data-monitor-actions="program"|"preview"]` for the Cast lane. That slot must retain at least a 44 px touch target without expanding the monitor row.

Regression evidence: `web/e2e/desk-viewport.spec.ts` measures document size, every source tile, monitor and bus/take control at 1920×1080, 1600×900 and 1366×768, with both 12 and 24 inputs. Set `DESK_SCREENSHOTS` to an existing directory to capture the twelve-input screenshots. Fixtures are explicitly labelled preview test charts, not live production evidence.
