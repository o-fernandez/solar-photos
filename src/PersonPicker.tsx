// The one person-picker combobox behind every "name or pick a person" surface:
// the People tile editor, the person page's rename / move / look pickers, the
// Lightbox face menu, and the review session's "someone else…". Each used to
// hand-roll the same input + match list + special rows + keyboard wiring; a
// behavior fix (like Enter defaulting to the top match) had to land six times.
//
// Behavior, shared everywhere:
//   * matches are the NAMED people whose name contains the query (the caller's
//     own person excluded), in the caller-provided order (biggest first);
//   * ↑/↓ + Enter via usePickerNav — Enter takes the highlighted row, the top
//     match by default, so "Cami" ⏎ picks Camila instead of minting "Cami";
//   * with `commitsText` (the rename combos) the highlight starts OFF the list:
//     Enter commits the typed text, arrows opt into the merge suggestions, and
//     blur commits too when `commitOnBlur` is set;
//   * mousedown on the list is prevented so a row click runs before the input's
//     blur can commit-and-close underneath it;
//   * callers add special rows (`leading` / `trailing`) — "+ New person",
//     "Just someone else", a click-only destructive "Ignore" (`nav: false`).
//
// The three `variant`s only pick which pre-existing CSS family the same DOM
// renders into; rows share the ns-face / ns-name / ns-count classes already.

import { useMemo } from "react";
import { faceCropUrl, type Cluster } from "./api";
import { usePickerNav } from "./pickerNav";

const VARIANTS = {
  /** Tile/header rename combobox (People grid, person page header). */
  combo: { wrap: "pname-combo", list: "name-suggest", item: "name-suggest-item", input: "pname-input" },
  /** Action-bar picker (selection bar, look bar, review focus). */
  bar: { wrap: "sb-picker", list: "sb-matches", item: "sb-match", input: "pname-input" },
  /** Face-menu popover (Lightbox). */
  menu: { wrap: "fm-move", list: "fm-matches", item: "fm-match", input: "pname-input fm-input" },
} as const;

/** A caller-provided row rendered alongside the person matches. */
export interface ExtraRow {
  key: string;
  /** Extra class on the row (e.g. "sb-new", "fm-new", "sb-neither", "danger"). */
  className?: string;
  content: React.ReactNode;
  onPick: () => void;
  /** false = click-only, outside ↑/↓/Enter reach (destructive rows). */
  nav?: boolean;
}

export default function PersonPicker({
  variant,
  className,
  people,
  excludeId = null,
  query,
  onQueryChange,
  placeholder,
  header,
  hint,
  showCounts = false,
  limit = 6,
  matchAll = false,
  commitsText = false,
  commitOnBlur = false,
  onCommitText,
  onPick,
  leading = [],
  trailing = [],
  onEscape,
}: {
  variant: keyof typeof VARIANTS;
  /** Extra class on the wrapper (e.g. the review card's "rf-picker"). */
  className?: string;
  people: Cluster[];
  /** The caller's own group — never offered as a target. */
  excludeId?: number | null;
  /** Controlled query text (callers usually need it for commit/new-row labels). */
  query: string;
  onQueryChange: (q: string) => void;
  placeholder: string;
  /** Optional list header ("Add to an existing person"). */
  header?: string;
  /** Optional hint line under the input (face-menu explainer). */
  hint?: string;
  showCounts?: boolean;
  limit?: number;
  /** true = an empty query lists everyone (move pickers); false = it lists
   *  nothing (rename combos, where the list is suggestions, not a browser). */
  matchAll?: boolean;
  /** Enter with nothing highlighted commits the typed text (rename combos). */
  commitsText?: boolean;
  commitOnBlur?: boolean;
  onCommitText?: () => void;
  onPick: (target: Cluster) => void;
  leading?: ExtraRow[];
  trailing?: ExtraRow[];
  onEscape: () => void;
}) {
  const v = VARIANTS[variant];

  const matches = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q && !matchAll) return [];
    return people
      .filter((c) => c.cluster_id !== excludeId && c.name)
      .filter((c) => (q ? c.name!.toLowerCase().includes(q) : true))
      .slice(0, limit);
  }, [people, query, excludeId, matchAll, limit]);

  // One flat render list; nav indices are assigned in render order to every
  // arrow-reachable row, skipping the click-only ones.
  let navIdx = 0;
  const withNav = (r: ExtraRow) => ({ ...r, navIndex: r.nav === false ? null : navIdx++ });
  const rows: {
    key: string;
    className?: string;
    content: React.ReactNode;
    onPick: () => void;
    navIndex: number | null;
  }[] = [
    ...leading.map(withNav),
    ...matches.map((m) => ({
      key: `person-${m.cluster_id}`,
      content: (
        <>
          <img className="ns-face" src={faceCropUrl(m.cover_face_id)} alt="" draggable={false} />
          <span className="ns-name">{m.name}</span>
          {showCounts && <span className="ns-count">{m.count.toLocaleString()}</span>}
        </>
      ),
      onPick: () => onPick(m),
      navIndex: navIdx++,
    })),
    ...trailing.map(withNav),
  ];

  const nav = usePickerNav(
    navIdx,
    (i) => rows.find((r) => r.navIndex === i)?.onPick(),
    { startUnselected: commitsText },
  );

  return (
    <div
      className={`${v.wrap}${className ? ` ${className}` : ""}`}
      onClick={(e) => e.stopPropagation()}
    >
      <input
        className={v.input}
        autoFocus
        value={query}
        placeholder={placeholder}
        onChange={(e) => {
          onQueryChange(e.target.value);
          nav.resetHighlight();
        }}
        onKeyDown={(e) => {
          if (e.key === "Escape") onEscape();
          else if (nav.onNavKey(e)) return;
          else if (e.key === "Enter" && commitsText) onCommitText?.();
        }}
        onBlur={commitOnBlur ? onCommitText : undefined}
      />
      {hint && <div className="fm-hint">{hint}</div>}
      {rows.length > 0 && (
        // preventDefault keeps the input from blurring (and commit-closing the
        // picker) before a row's click handler runs.
        <ul className={v.list} onMouseDown={(e) => e.preventDefault()}>
          {header && <li className="name-suggest-head">{header}</li>}
          {rows.map((r) => (
            <li
              key={r.key}
              className={`${v.item}${r.className ? ` ${r.className}` : ""}${
                r.navIndex != null && nav.highlight === r.navIndex ? " hi" : ""
              }`}
              onMouseEnter={r.navIndex != null ? () => nav.setHighlight(r.navIndex!) : undefined}
              onClick={r.onPick}
            >
              {r.content}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
