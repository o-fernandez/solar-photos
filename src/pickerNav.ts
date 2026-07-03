// Keyboard navigation for the person-picker typeaheads (move-to / someone-else /
// name comboboxes). One shared behavior everywhere:
//   * ↑/↓ move the highlight through the rows,
//   * Enter activates the highlighted row,
//   * with `startUnselected` the highlight starts OFF the list (Enter means
//     "commit what I typed"; arrows opt into the list).
// The crucial fix over the old per-picker handlers: Enter used to CREATE a new
// person from a half-typed query even while the person you meant sat at the top
// of the matches — now the top match is the default.

import { useEffect, useState } from "react";

export function usePickerNav(
  rowCount: number,
  onActivate: (index: number) => void,
  opts: { startUnselected?: boolean } = {},
) {
  const base = opts.startUnselected ? -1 : 0;
  const [highlight, setHighlight] = useState(base);

  // Keep the highlight valid as the query filters the rows (and back in range
  // when rows reappear).
  useEffect(() => {
    setHighlight((h) => Math.max(base, Math.min(h, rowCount - 1)));
  }, [rowCount, base]);

  const resetHighlight = () => setHighlight(base);

  /** Handle a keydown from the picker's input. Returns true when consumed. */
  const onNavKey = (e: React.KeyboardEvent): boolean => {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setHighlight((h) => Math.min(rowCount - 1, h + 1));
      return true;
    }
    if (e.key === "ArrowUp") {
      e.preventDefault();
      setHighlight((h) => Math.max(base, h - 1));
      return true;
    }
    if (e.key === "Enter" && highlight >= 0 && rowCount > 0) {
      e.preventDefault();
      onActivate(Math.min(highlight, rowCount - 1));
      return true;
    }
    return false;
  };

  return { highlight, setHighlight, resetHighlight, onNavKey };
}
