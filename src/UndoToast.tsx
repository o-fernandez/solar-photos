// The one bottom-center toast for a just-applied action: a label, and — when
// the action is revertable — an Undo button. Every surface that flashes one
// (grid hide, person-page corrections, viewer corrections, plain notices)
// renders this instead of hand-rolling the same two elements.

export default function UndoToast({
  label,
  onUndo,
}: {
  label: React.ReactNode;
  /** Omit for a plain, button-less notice. */
  onUndo?: () => void;
}) {
  return (
    <div className="undo-toast">
      <span>{label}</span>
      {onUndo && (
        <button className="undo-btn" onClick={onUndo}>
          Undo
        </button>
      )}
    </div>
  );
}
