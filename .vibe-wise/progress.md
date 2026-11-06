# Learning Progress

## Agent door (scene file)
- Introduced (AI explained, 31 Oct 2026): how agents build a scene today (JSX with one `app.run` command per step, or a recipe JSON that builds a new comp) compared with a possible scene file that is edited in place.
- Demonstrated understanding: none recorded yet.
- Needs reinforcement: not assessed.
- 31 Oct 2026, Elias's own question: does naming eases ("Expo Out 1") remove customizability? He spotted the trade-off himself. AI explained: a name is a link to a saved curve (his Graphs cards are already name + numbers), raw numbers and one-off tweaks stay possible; reference vs copy explained.

## Rewrite vs scene file
- 31 Oct 2026, Elias asked whether the scene file is better than the rewrite and whether to test first. AI explained they solve different problems (scene file = how agents write and change scenes; core = playback speed), that MEM4 (dfce6c19, 30 Oct, untested in the app) targets one measured cause of lag, and proposed two tests (lag on a copy of his project; one real scene as a scene file vs today on 3 fixes). Proposal only.

- 31 Oct 2026: Elias parked performance ("lets skip performance fro now, focus on design/ideas"). The lag tests are parked, not decided.

## Pending decision (scene flow)
- Decision: how Elias wants a scene to go from his request to done (what he says, what the agent does, what he sees, how he fixes it). Stage: awaiting reasoning. Open design questions listed for him: what one file holds, file vs comp as the truth, words as anchors, linked vs copied names, how a fix is described, whether he ever sees the file.

## Pending decision (eases by name)
- Decision: when Elias later changes a saved curve, should scenes that used it change too, or keep the curve as it was when built? Stage: awaiting reasoning.

## Pending decision
- Decision: engine rewrite direction. Stage: awaiting reasoning.
- The AI proposed (not decided by Elias): rewrite the core and add a scene-file "agent door", keep the effects, GPU, text and codec crates, and run a small test on one real scene first. Elias answered the outcome questions only (agents build and he steers, drop AE parity, smooth playback, full rewrite). How a scene file should look and behave is his to reason through.
