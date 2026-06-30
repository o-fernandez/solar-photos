# Spec: Face corrections — name, reassign, ignore

Status: **draft for review.** Two product forks decided 2026-06-30 (see §9):
core-only naming, per-instance ignore.
Author: working session, 2026-06-30.

## 1. Why

Today the People grid clusters faces well but gives the user almost no way to
*correct* it, and the one correction we do have quietly makes things worse. Three
real situations have no good answer:

- **A cluster is contaminated.** Camila's group contains a chunk of *Max* — a
  different baby — because baby faces sit very close in embedding space. The user
  can see it instantly; the software can't.
- **The contamination gets *welded in* the moment you name the person.** This is
  the dangerous one. `name_cluster` binds **every face currently in the cluster**
  to one durable identity (`db.rs` `ensure_identity_for_cluster` →
  `UPDATE faces SET identity_id = ? WHERE cluster_id = ?`). From then on
  `apply_must_links` *forces* those faces — Max included — back together on every
  re-cluster. So while a cluster is unnamed, contamination can drain out over
  time (a from-scratch re-cluster separates the babies as evidence accumulates);
  the instant you name it, the contamination is permanent. And our own "Still
  finding people" banner actively invites naming *during the sweep*, which is
  exactly when clusters are noisiest.
- **The only correction we have orphans instead of reattributing.** The per-photo
  ✕ ("Not this person") sets `cluster_id = NULL`. Max's face disappears from
  People entirely — it does *not* join Max's real group — and it's one click per
  photo, with no way to say "all of these are Max." It also records no
  `cannot_link`, so a full rescan can resurrect the mistake.

This spec defines one coherent **face-correction** capability that fixes the
weld, makes corrections reattribute rather than orphan, makes them durable, and
exposes the same primitives from two places: the **person page** (acting on a
known person's photos) and the **open photo** (acting on whatever faces are in
front of you). It deliberately excludes "add a face we didn't detect" (§10).

## 2. Ground truth: how durability works today

Any correction has to respect the existing model, so it's worth stating exactly.

| Thing | Lifetime | Meaning |
|---|---|---|
| `faces.cluster_id` | **Transient** — rebuilt from scratch on every re-cluster | Which group a face is in *right now*. Not stable across re-clusters. |
| `faces.identity_id` | **Durable** | The **must-link**. Every face sharing an identity is forced into one cluster by `apply_must_links`. This is what survives re-clustering. |
| `identities.name` | Durable, lives on the identity | The person's name. After a re-cluster it's re-anchored to whichever new cluster holds the plurality of the old named cluster's faces. |
| `cannot_link(a, b)` | Durable, an **identity** pair | "Not the same person." **Today this only suppresses merge/growth *suggestions* — it is NOT enforced inside `recluster` or `assign`.** |

Two consequences fall out of that table and shape everything below:

1. **A durable correction must touch `identity_id`, never just `cluster_id`.**
   Anything written only to `cluster_id` evaporates at the next re-cluster.
2. **`cannot_link` does not currently keep two people apart.** It stops us from
   *suggesting* a bad merge, but the embedding clusterer can still physically put
   a Camila face and a Max face in one cluster because they're close in feature
   space. That makes "move Max out and keep him out" impossible to honor today.
   Fixing this is foundational (§4), not optional polish.

## 3. The model: one primitive, three intents

Every correction is an action on a **set of faces** `F` (one face, or many via
selection). There are exactly three intents:

- **Name** — `F`'s person is unnamed; give them a name. (Already exists for whole
  clusters; we extend the *surface*, not the mechanic.)
- **Reassign** — `F` belongs to a *different* person than the cluster says. Move
  them to that person (existing or brand-new) and bar them from coming back.
- **Ignore** — `F` isn't a person we want tracked at all (a stranger in the
  background, a face on a poster, a reflection). Remove from People, durably,
  and never re-cluster.

Naming the primitive "act on a set of faces" is what lets the person page and the
open photo share one implementation: they differ only in how `F` is selected.

## 4. Foundational fix — enforce `cannot_link` in clustering

Before reassign can promise "they won't come back," the clusterer has to honor
`cannot_link` as a hard constraint, not a suggestion filter.

- **Batch `recluster`:** thread the cannot-link identity pairs into the
  agglomeration. The complete-linkage step already tests every cross-pair before
  a merge; add one rule: **refuse a merge if the union would co-locate a
  cannot-linked identity pair.** Same shape as the existing purity guard, one
  more predicate. After `apply_must_links`, assert no surviving cluster violates a
  cannot-link (it can't, if the merge guard held).
- **Incremental `assign`:** when voting for a target cluster, skip any cluster
  whose identity is cannot-linked to the incoming face's identity. (In practice
  most live-scanned faces have no identity yet, so this is a thin guard; the batch
  path is where it earns its keep.)
- **Conflict handling:** a user could merge A+B (must-link) and later cannot-link
  them. Treat the *most recent* explicit action as truth: cannot-link removes any
  must-link that binds the two, and vice-versa. Surface nothing; just resolve.

Without this, §5 silently fails for exactly the embedding-close pairs (babies,
siblings) that need it most.

## 5. Reassign — "this isn't X / this is Y"

The core new correction. Acts on `F` (currently in cluster `A`, identity `I_A`),
targeting a person `P`.

**Target options:**
- An **existing person** — pick `I_B` from the named people (and recent unnamed
  groups). Typeahead, same component as the naming combo.
- A **new person** — split `F` off into a fresh identity, optionally named on the
  spot.

**Effect (all durable):**
1. Ensure `A` has an identity `I_A` (`ensure_identity_for_cluster`).
2. Bind `F`: `identity_id = I_B`, `cluster_id = B`'s current cluster. (Must-link
   to the target — this is the reattribution, the thing the ✕ never did.)
3. Record `cannot_link(I_A, I_B)`. With §4 live, Max never re-merges into Camila
   even though their embeddings are close.

**UX:**
- **Single face:** in the open photo or a person cell, "Not Camila → " opens the
  target picker. One step.
- **Many faces (the Max chunk):** multi-select in the person page (shift/▢ on
  cells), then one action: "Move 32 photos to → {new person | existing person}."
  This is the elegant answer to the journey in the brief — 32 photos, one
  decision, and they're correctly filed under Max instead of vanishing.
- **Undo:** same single-level undo toast the ✕ already uses, extended to carry
  the full before-state (faces' prior `identity_id`/`cluster_id` + the
  cannot-link we added) so undo is exact.

The existing per-photo ✕ stays as the fast path for one-offs, but is
re-implemented on top of reassign-to-ignore semantics (§6) so it stops orphaning.

## 6. Ignore — "this isn't a person I track"

- **Mechanic:** add an explicit `faces.ignored` flag (don't overload
  `cluster_id = NULL`, which the sweep's "short of a full rescan" caveat makes
  leaky). An ignored face has `ignored = 1`, `cluster_id = NULL`,
  `identity_id = NULL`, and is excluded from: cluster/person queries, the
  `recluster` input set, and the incremental `assign`/sweep — so it is never
  resurrected, even by a full rescan.
- **Reversible:** un-ignore clears the flag; the face rejoins the clustering pool
  on the next pass.
- **Scope (decided):** per-instance by default — ignore only this one detection.
  When the face belongs to a multi-face group, offer "ignore just this / ignore
  this whole person" inline. We never silently hide more than the user pointed at.

## 7. Naming — bind a core, not the contamination (fixes §1's weld)

This is the change that makes early naming safe. **Decided: core-only.**

**Problem restated:** naming must-links the *entire* cluster. That defends
against over-splitting (the documented core failure mode — naming a person and
having a re-cluster shatter them and lose the name on most fragments), but it
also welds in any contamination present at naming time.

**Proposed:** when naming, must-link only a **confident core** — faces within
`τ_core` cosine of the cluster's high-confidence centroid — and leave outliers
*in the cluster* (so the count and photos still show) but **identity-free**. Then:

- The core stays glued, so naming still defends against over-splitting.
- Outliers are no longer welded, so a later re-cluster can shed Max, and the
  magnet re-attracts genuine stragglers.
- Name re-anchoring is unaffected: it tallies *all* faces of the old named
  cluster (not just identity-bound ones), so the name still follows the core's
  plurality.

`τ_core` is a new tunable; start conservative (only obvious outliers escape) and
measure.

### 7a. Safety invariant — **naming never silently ejects a present face**

Core-only naming has a failure mode that would feel like a *bug worse than the
one it fixes*: you name Camila, a re-cluster runs, decides a few of her genuine
photos are outliers (a bad angle, hard light), and **sheds them out of the named
group.** From the user's seat that reads as "I named her and the app started
deleting her photos." That breaks naming's core promise — that naming makes a
person *stable*. We must make it structurally impossible.

The rule is asymmetric. Core-only naming changes only what the magnet is allowed
to *pull in or out over time* — it must **never actively eject a face that was
already in the named cluster at naming time.** Concretely:

- **Sticky membership, loose weld.** The core is a hard must-link. Every *other*
  face present at naming time gets a weaker **"stays unless overruled" tie**: a
  re-cluster may not forcibly relocate it on embedding drift alone. It is not
  welded (so it isn't contamination-locked like §1), but it is *sticky* (so a
  genuine Camila straggler never just wanders off).
- **A present face may leave a named person only via two doors, never silently:**
  1. **The user moves it** (reassign/ignore, §5/§6) — always wins.
  2. **The system is confident it's someone else** — e.g. the face's strongest
     match is decisively another identity, clearing a *high* bar well above
     `τ_core`. Even then it is **surfaced, not silent**: "Moved 3 photos that look
     like a better match for <person>" with one-tap undo — the same
     confirm-don't-surprise contract as our merge prompts (Principle 5).
- **Default-deny on uncertainty.** If the system isn't confident a present face
  belongs elsewhere, it *stays*. Ambiguity resolves toward keeping the named
  person whole, never toward quietly shrinking it.

Implementation hook: model the two tiers explicitly — `identity_id` (hard
must-link = core) plus a softer membership marker for the rest of the named
cluster's faces (e.g. a `pinned_identity` / sticky flag) that `recluster` treats
as "keep here unless a high-confidence cross-identity match fires, and emit a
surfacing event when it does." A named cluster shrinking is always either
user-driven or an announced, undoable suggestion — never a surprise.

## 8. Surfaces

Same three primitives, two entrances. Both build on a new
`faces_in_photo(photo_id)` query → `[{ face_id, box, cluster_id, name|null }]`.

### 8a. Person page (`PersonView`) — the "known person" entrance
- Multi-select cells → bulk **reassign** / **ignore** (§5, §6).
- Keep the per-cell ✕ for one-offs (now non-orphaning).
- This is where the Max-chunk journey lives.

### 8b. Open photo (`Lightbox`) — the "this photo" entrance
`Lightbox` is **face-blind today** (`index/total/resolveId/onClose` only;
`PhotoDetail` carries just filename + timestamp). New work:
- Fetch faces for the current photo; draw subtle boxes/chips over each face,
  labelled with the person's name or "Unnamed."
- Per face, a small menu: **Name** (unnamed), **Rename / This is someone else**
  (reassign, §5), **Ignore** (§6).
- Honors Principle 2: overlays appear only for the photo in focus; nothing in the
  grid behind reflows.
- Shared by Timeline and the person page (both already open `Lightbox`), so one
  build covers "I'm anywhere, I opened a photo, let me fix the faces in it."

## 9. Decisions (settled 2026-06-30)

**(a) "Ignore" scope → per-instance, offer group.** Ignore only the detection the
user pointed at; when it's part of a multi-face group, offer "ignore this whole
person" inline. Honest (we only touch what we've seen) without being tedious for
the recurring-stranger case. Detail in §6.

**(b) Naming weld → core-only, with a hard safety invariant.** Naming binds only
the confident core, not the whole cluster. The whole-cluster weld is the root
cause behind the brief's question (1) and it fights the user during the very sweep
we tell them to name in. Detail in §7. **Non-negotiable constraint (§7a):** naming
must *never silently eject a face already in the named cluster* — a present face
leaves only by user action or an announced, undoable suggestion. Core-only
naming is only acceptable built this way. (Cost: a `τ_core` computation + a sticky
membership tier + one new tunable.)

## 10. Out of scope (for now)

- **"Add a face we didn't detect."** Requires re-running detection on a photo and
  a manual-box flow; different machinery, deferred by the brief.
- Bulk cross-person operations beyond reassign/ignore (e.g. "split this cluster
  into three") — revisit if reassign proves too granular at scale.

## 11. Principles check

- **#2 (never reflow):** corrections update counts and grids, so apply them on
  explicit user action with optimistic local removal + undo, never a background
  reflow under the user's eyes. Lightbox overlays are scoped to the focused photo.
- **#5 (automate the tedious, confirm in batches):** multi-select reassign is the
  batch correction; we never make the user click 32 times.
- **#6 (degrade at scale):** `faces_in_photo` is a single indexed lookup;
  cannot-link enforcement is O(pairs) inside an agglomeration we already run.

## 12. Suggested sequencing

1. **§4 enforce `cannot_link`** in `recluster` (+ `assign`). Foundational; nothing
   below is durable without it. Ships invisibly (no UI), immediately makes the
   existing "Not the same" prompts actually stick.
2. **§5 reassign** + multi-select in `PersonView`. Solves the brief's Max journey.
3. **§6 ignore** with the explicit `ignored` flag; re-base the ✕ on it.
4. **§8b Lightbox** face overlay — the in-photo surface for all three.
5. **§7 core-only naming** — the deepest change; do it once (1)–(3) prove the
   correction model out, so naming has a safety net while we retune the weld.
