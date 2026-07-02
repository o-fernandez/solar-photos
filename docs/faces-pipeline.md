# Face recognition pipeline

How Solar turns 30k photos into named people, and — more importantly — how it
handles the cases the face model *can't* cleanly separate (babies, siblings,
look-alikes). This is the hard-won design rationale; read it before changing
anything in `src-tauri/src/{cluster,db,lib}.rs` or `src/{People,PersonView}.tsx`.

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

- **cluster** — a group of faces produced by (re-)clustering. Cluster ids are
  reassigned from scratch on every re-cluster; a "person tile" in the UI is a cluster.
- **identity** — a durable person record (`identities` table). Survives re-clusters
  and carries the name. A face's `identity_id` is the **must-link**.
- **`faces.confirmed`** (0/1) — **the key flag.** `1` = the *user* vouched for this
  face (named / moved / merged). `0` = the machine auto-folded it (tentative).
  - Only **confirmed** faces are must-links (`confirmed_face_identities`) and anchor
    exemplars (`confirmed_anchor_embeddings`). Auto-folded faces are never welded on —
    they're re-decided every pass. This is what makes wrong folds self-correcting.
- **cannot_link** — durable "these two identities are not the same person."

## The pipeline (per re-cluster, `run_recluster`)

1. **Cluster** all faces by embedding (`cluster::recluster`, purity-biased
   complete-linkage over a mutual-kNN graph, `TAU_LINK ≈ 0.5`), honoring:
   - **must-links** from **confirmed** faces only, and
   - **cannot-links** between identities.
   Auto-folded (unconfirmed) faces are *free* here — they re-group by appearance.
2. **Re-derive cluster names** from the durable **identity** whose confirmed faces
   landed in each cluster (read fresh, post-clustering — so a name added *during*
   the pass isn't wiped by a stale snapshot; that was a real bug once).
3. **Auto-fold** (`auto_fold_confident`) — the competitive assignment (below).

Naming / merging / absorbing / detaching all trigger `run_recluster` (with a
pending-rerun guard so rapid actions aren't dropped). That re-cluster is what makes
the system **self-heal**: naming a second look-alike frees the tentative faces and
re-decides them, ejecting the first person's wrongly-folded faces.

## Competitive assignment (the intelligence)

`cluster_identity_matches` scores every candidate cluster against **each confirmed
identity's** dense-core anchor (`identity_candidates`), best-first. Then per cluster:

- Assign to the **decisively best** identity: top match must clear `AUTO_FOLD_MIN`
  **and** beat the runner-up by `AUTO_FOLD_MARGIN`. A **near-tie** (two babies both
  plausible) is **ambiguous → left for review, never guessed.**
- **Two floors, on purpose:**
  - `COMPETITOR_MIN` (=1): any confirmed group can **compete** — push a look-alike
    *off* someone (into review). Cheap, safe, and lets a one-group "not X" defend
    immediately.
  - `MIN_ANCHOR` (=4): only a substantial identity can **absorb** (claim) a cluster.
    Prevents a thin/one-face "person" from vacuuming a swarm (the original footgun).
- **Anchor hygiene** (`anchor_core`): match against an identity's *dominant* core
  (outliers dropped) so one bad fold can't poison the anchor and cascade.

The review list (`get_identity_growth`) uses the same competitive matrix: it won't
keep suggesting a cluster that's decisively **someone else's**.

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
  (`cluster::group_looks` leader-clustering + centroid merge). Two jobs: **filter**
  the grid by look, and **repair** — a look whose centroid matches another named
  person's anchor is flagged "looks like X" for a one-click batch move. A look that's
  ~the whole person (`LOOK_SHARE_MAX`) is suppressed (it's just "All").
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

## Tuning knobs (all in `lib.rs` unless noted)

| Const | Val | Meaning |
|---|---|---|
| `TAU_LINK` (cluster.rs) | 0.5 | link threshold for unsupervised clustering |
| `AUTO_FOLD_MIN` | 0.6 | min similarity to auto-fold (below → review) |
| `AUTO_FOLD_MARGIN` | 0.06 | best must beat runner-up by this, else ambiguous → review |
| `MIN_ANCHOR` | 4 | confirmed faces needed to *absorb* / suggest / flag |
| `COMPETITOR_MIN` | 1 | confirmed faces needed to *compete* defensively |
| `ANCHOR_CORE_TAU` | 0.5 | grouping to find an anchor's dominant core |
| `LOOK_TAU` / `LOOK_MERGE` | 0.5 / 0.55 | looks: sub-cluster then merge similar |
| `LOOK_ABS_MIN` / `LOOK_PCT` | 10 / 0.05 | a look must be this big absolutely and relatively |
| `LOOK_FLAG_ABS` / `LOOK_FLAG_MARGIN` | 0.5 / 0.08 | when to flag a look as another person |

## Known limitations / future work

- **Two *confirmed* people whose faces are embedding-adjacent can still share a
  cluster.** Must-links pull each person together but don't push two people *apart*.
  Fix: treat distinctly-named identities as implicitly cannot-linked during clustering.
- **Naming triggers a full re-cluster** (seconds on 30k). That's the price of
  self-heal. If it gets sluggish, move to **identity-centric grouping** so auto-fold
  sets `identity_id` for display without merging `cluster_id` — then self-heal is a
  cheap re-derive, no full re-cluster.
- **Competition favors larger exemplar sets** (uses best-match `max_sim`). A person
  with few confirmed faces can lose their own faces to a person with many. Confirming
  a few more usually tips it; a size-invariant metric (centroid / kNN vote) is the
  cleaner fix.
- **True negative exemplars** (penalize similarity to rejected faces in the score)
  were considered as an alternative to the durable-competitor approach; competition
  subsumes most of it, but it's the more direct model if needed.
