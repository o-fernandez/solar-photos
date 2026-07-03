# Face recognition pipeline

How Solar turns 30k photos into named people, and — more importantly — how it
handles the cases the face model *can't* cleanly separate (babies, siblings,
look-alikes). This is the hard-won design rationale; read it before changing
anything in `src-tauri/src/{cluster,recognition,db,lib}.rs` or `src/{People,PersonView}.tsx`.

## The problem, honestly

- Detection + embeddings (YuNet + SFace) are good for **adults**, and unreliable
  for **infants**: two different babies of similar age/family often sit as close
  in embedding space as one baby to themselves. **No threshold, anchor trick, or
  clustering parameter fixes non-separable vectors.** So the goal is not "sort
  everyone automatically" — it's: do the separable ~80% automatically, **never
  silently corrupt** the hard part, and make the human-taught part fast and durable.
- Unsupervised clustering is deliberately **purity-biased** (errs toward *not*
  mixing two people), which means each person is **over-split** into many small
  fragments. Reuniting those is the system's job, not the user's.

## Data model

Two layers, joined by one derived key:

- **cluster** (appearance layer) — a group of faces produced by unsupervised
  (re-)clustering. Cluster ids belong to the clustering passes alone: corrections
  and auto-folds never touch them, and they're renumbered only by a full
  re-cluster (sweep-drain consolidation, reset, migration, the manual command).
- **identity** (person layer) — a durable person record (`identities` table).
  Survives everything and carries the name. A face's `identity_id` is its person;
  when confirmed it's also the **must-link** the re-cluster honors.
- **display group** = `COALESCE(-identity_id, cluster_id)` (`db::GROUP_EXPR`) —
  what a "person tile" actually is. A face with an identity shows under the
  **negative, forever-stable** key `-identity_id`; an unassigned face under its positive
  appearance cluster. No merge ever has to be un-done to move a face between
  tiles: only the identity layer is written.
- **`faces.confirmed`** (0/1) — **the key flag.** `1` = the *user* vouched for this
  face (named / moved / merged). `0` = the machine auto-folded it (tentative).
  - Only **confirmed** faces are must-links (`confirmed_face_identities`) and anchor
    exemplars (`confirmed_anchor_embeddings`). Auto-folded faces are never welded on —
    they're re-decided every pass. This is what makes wrong folds self-correcting.
- **cannot_link** — durable "these two identities are not the same person."

## The two passes

**Self-heal (`run_auto_fold`, the frequent one).** Naming / merging / absorbing /
detaching trigger a **debounced** fold pass (`schedule_refold`, `REFOLD_DEBOUNCE`
of quiet; plus the pending-rerun guard so rapid actions aren't dropped). It wipes
every *tentative* identity assignment and re-derives them competitively against
the confirmed anchors (`auto_fold_confident`). Because a fold writes only
`identity_id` — never `cluster_id` — this is a cheap identity-layer pass: naming
a second look-alike re-decides the tentative faces and ejects the first person's
wrongly-folded ones with **no re-cluster at all**.

**Re-cluster (`run_recluster`, the rare one).** Rebuilds the appearance layer from
embeddings (`cluster::recluster`, purity-biased complete-linkage over a mutual-kNN
graph, `TAU_LINK ≈ 0.5`), honoring **must-links** from confirmed faces and
**cannot-links** between identities (named pairs implicitly). Runs on sweep-drain
consolidation, reset, migrations, and the manual command — never as the price of a
correction. Identity groups (names, confirmations, tiles) are keyed by durable
identity ids and pass through untouched; it ends with a fold pass so tentative
assignments reflect the fresh clusters.

### Suggestion cache + generation guard

Merge suggestions and growth cards are **computed once at the end of each pass**
(`refresh_suggestion_cache`, on the pass's own thread + connection) and served
from a cached snapshot — never recomputed per People-tab open (the old way held the
shared DB lock through seconds of matrix math, stalling every avatar request). The
monotonic **cluster generation** bumps only when a re-cluster renumbers the
positive appearance keys; payloads carry it and mutations verify it via
`ensure_generation`. Identity keys are negative and never invalidated, so with the
two-layer model the guard only ever fires around the rare consolidation pass —
mid-review folds no longer strand a session.

## Competitive assignment (the intelligence)

`cluster_identity_matches` scores every candidate group against **each confirmed
identity's** dense-core anchor (`identity_candidates`), best-first. Candidates are
the *positive* groups only — a negative group IS a person (or competitor), and
identities merge only by explicit user action. Then per candidate:

- Assign to the **decisively best** identity: top match must clear `AUTO_FOLD_MIN`
  **and** beat the runner-up by `AUTO_FOLD_MARGIN`. A **near-tie** (two babies both
  plausible) is **ambiguous → left for review, never guessed.** The assignment sets
  `identity_id` (tentative, `confirmed = 0`) and nothing else — the cluster keeps
  its id, so the next pass can re-decide it for free.
- **Two floors, on purpose:**
  - `COMPETITOR_MIN` (=1): any confirmed group can **compete** — push a look-alike
    *off* someone (into review). Cheap, safe, and lets a one-group "not X" defend
    immediately.
  - `MIN_ANCHOR` (=4): only a substantial identity can **absorb** (claim) a cluster.
    Prevents a thin/one-face "person" from vacuuming a swarm (the original footgun).
- **Anchor hygiene** (`anchor_core`): match against an identity's *dominant* core
  (outliers dropped) so one bad fold can't poison the anchor and cascade.

The review list (`get_identity_growth`) uses the same competitive matrix: it won't
keep suggesting a cluster that's decisively **someone else's**, never offers a
cluster holding *anyone's* confirmed faces (merging two named people is only ever
the explicit rename/typeahead path), and scores with a **mean-of-top-K** anchor
match (size-invariant — a 6-exemplar baby no longer loses to a 900-exemplar adult).

## Same-photo exclusion (the free signal)

Two faces in one photo are two different people — the strongest signal available
for sibling babies the embeddings can't separate, and it's immune to bad metadata
(a scanned print lies about its date, not about who's in the frame together).
Enforced everywhere:
- **Clustering**: `LinkConstraints.photo_of` — a merge whose sides share a photo is
  refused (exception: `same_photo_ok`, box pairs with IoU ≥ `DOUBLE_DETECTION_IOU`
  = one face detected twice). Same rule in the incremental `ClusterIndex::assign`.
- **Auto-fold / growth / pairwise suggestions**: a candidate that co-occurs with the
  identity's confirmed photos (or with the other cluster) is vetoed (`cooccurs`).
- **The collage escape hatch** (same person twice in one image — collage, mirror,
  booth strip): never guessed by similarity (identical twins can look near-duplicate
  too). When the pairwise engine finds *strong* same-person evidence
  (≥ `SAME_PHOTO_ASK_MIN`) between co-occurring clusters — the contradiction — it
  raises a `SamePhotoTwin` review card showing the **shared photo**; a human tells a
  collage from twins in one glance. "Same person" writes durable per-face-pair
  exceptions to `same_photo_ok` (photo-level truth: kept across "start people over",
  wiped only by full re-detection) and merges; "two people" writes the cannot-link.
  Without this, collage fragments were silently quarantined from every automatic
  reunion path forever.

## The review queue + focus mode

Every engine's output is normalized into one payoff-sorted `ReviewQueue`
(`build_review_queue`, cached per generation, served by `get_review_queue`):
strong batches, uncertain "maybe" groups, **who-is-this** cards (clusters claimed
by 2+ named people — the near-ties auto-fold refuses to guess; answering teaches
the winner and starves the loser, the highest information-per-click question), and
pairwise evidence. The People banner is just the entry point; `ReviewFocus.tsx`
walks the queue one decision per screen (Y/N/S keys, skip, session tally). The
session works on a snapshot — if a re-cluster completes mid-session, the next
answer is refused by the generation guard and the session ends cleanly rather than
acting on renumbered ids.

## Negatives that generalize ("not <person>")

Marking a review candidate **"not Mía"** does *not* just record a cannot-link (that's
per-group and inert). It calls `not_this_person`, which makes the rejected group a
**durable competitor**: its faces become their own **confirmed** identity ("someone
else"), cannot-linked from the person. Because confirmed identities *compete*, other
look-alikes now get pulled toward that competitor and **off** the person — the
rejection **generalizes**. Name that competitor later and it becomes a full magnet.

The mental model: **the system learns identity by competition, not negation.** The
strongest way to teach "not Mía" is to teach "is Carolina" (positive label on the
competitor); `not_this_person` is the shortcut that mints an unnamed competitor for you.

## Person page

- **Looks strip** (`get_person_looks`) — coarse appearance sub-clusters of one person
  (`cluster::group_looks` leader-clustering, **raw** — no centroid merge). Two jobs:
  **filter** the grid by look, and **repair** — a look whose centroid matches another
  named person's anchor is flagged "looks like X" for a one-click batch move.
  - **Why no merge / no relative floor** (learned the hard way, measured on a real
    5k-face person): within one person the look centroids sit at 0.70–0.95 cosine —
    identity embeddings are *trained* to erase vibes — so any centroid-merge threshold
    chains every era into one blob transitively (the same single-linkage failure the
    main clusterer exists to prevent). The blob then trips the "~whole person"
    suppression, the small precious eras (childhood!) trip the 5%-of-person relative
    floor, and the strip shows nothing. Raw fine looks + an absolute floor
    (`LOOK_ABS_MIN`) + a cap (`MAX_LOOKS`) is what actually surfaces "kid Omar".
  - **Never cluster looks by date**: scanned/photographed old prints carry the scan
    date, not the capture date (kid-Omar's metadata spans 2004–2026). Appearance
    grouping collects them correctly anyway; time is a display hint at most.
- **Actions** on a selected look or multi-selection: move to a person, split to a new
  person, **"Not <name>"** (`detach_faces` — clear identity, scatter, re-home by
  appearance), **"It's <name>"** (dismiss a bad flag via cannot-link), ignore.
- **Rename** shows the merge-into-existing typeahead; grid + page behave the same.

## Reset

- **"Start people over"** (`reset_face_decisions`, in Settings) — the fast one:
  keeps detected faces + embeddings, clears all names/groups/decisions, re-clusters
  from scratch. **Backs up the DB first** (`<db>.pre-reset.bak` via `VACUUM INTO`).
- `reset_face_recognition` — the nuke: deletes faces and re-runs the full detection
  sweep. Not wired to the UI.

## Tuning knobs (all in `recognition.rs` unless noted)

| Const | Val | Meaning |
|---|---|---|
| `TAU_LINK` (cluster.rs) | 0.5 | link threshold for unsupervised clustering |
| `AUTO_FOLD_MIN` | 0.6 | min similarity to auto-fold (below → review) |
| `AUTO_FOLD_MARGIN` | 0.06 | best must beat runner-up by this, else ambiguous → review |
| `MIN_ANCHOR` | 4 | confirmed faces needed to *absorb* / suggest / flag |
| `COMPETITOR_MIN` | 1 | confirmed faces needed to *compete* defensively |
| `ANCHOR_CORE_TAU` | 0.5 | grouping to find an anchor's dominant core |
| `LOOK_TAU` | 0.5 | looks: leader-cluster threshold (raw, no merge pass) |
| `LOOK_ABS_MIN` / `MAX_LOOKS` | 10 / 8 | absolute look floor / genuine looks shown |
| `LOOK_FLAG_ABS` / `LOOK_FLAG_MARGIN` | 0.5 / 0.08 | when to flag a look as another person |
| `REFOLD_DEBOUNCE` | 4s | quiet period before a correction's self-heal fold runs |

## Known limitations / future work

- ~~Naming triggers a full re-cluster~~ — **done** (2026-07): identity-centric
  grouping landed. Auto-fold writes `identity_id` only, self-heal is a cheap
  re-derive, and the appearance layer is rebuilt only on consolidation. Two
  embedding-adjacent confirmed people can still share an *appearance* cluster, but
  the display splits them by identity, so nothing user-visible mixes.
- **`cluster_identity_matches` is O(identities × library)** — every confirmed
  identity (including each unnamed "not X" competitor, which lives forever) costs a
  full-library clone + anchor matmul per fold pass. Fine at today's counts; the fix
  when it bites is building the candidate matrix once and scoring all anchors
  against it in one pass, plus expiring competitors whose faces were since claimed.
- **Competition favors larger exemplar sets** (uses best-match `max_sim`). A person
  with few confirmed faces can lose their own faces to a person with many. Confirming
  a few more usually tips it; a size-invariant metric (centroid / kNN vote) is the
  cleaner fix.
- **True negative exemplars** (penalize similarity to rejected faces in the score)
  were considered as an alternative to the durable-competitor approach; competition
  subsumes most of it, but it's the more direct model if needed.
