import { useState } from 'react';

export interface ScanFormValues {
  root: string;
  hidden: boolean;
  targetPaths: Record<string, string>;
}

// The nine asset kinds with their built-in default destination format
// strings (DESIGN.md "Destination Path Format Strings > Defaults"); the
// defaults double as input placeholders. Empty input = use the configured
// (or built-in) default for that kind.
const KINDS: Array<[string, string]> = [
  ['document', 'Documents/%f'],
  ['photo', 'Pictures/%Y/%m/%f'],
  ['video', 'Videos/%Y/%m/%f'],
  ['audio', 'Music/%f'],
  ['repository', 'Projects/%f'],
  ['archive', 'Archives/%f'],
  ['data', 'Data/%f'],
  ['executable', 'bin/%f'],
  ['other', 'Other/%f'],
];

interface ScanFormProps {
  defaultRoot: string;
  defaultHidden?: boolean;
  defaultTargetPaths?: Record<string, string>;
  hasSuggestions: boolean;
  startDisabled?: boolean;
  onStart: (values: ScanFormValues) => void;
  onResume: () => void;
}

export function ScanForm(props: ScanFormProps) {
  const {
    defaultRoot,
    defaultHidden,
    defaultTargetPaths,
    hasSuggestions,
    startDisabled,
    onStart,
    onResume,
  } = props;
  const [root, setRoot] = useState(defaultRoot);
  const [hidden, setHidden] = useState(defaultHidden ?? false);
  const [paths, setPaths] = useState<Record<string, string>>(() => {
    const init: Record<string, string> = {};
    for (const [kind] of KINDS) {
      init[kind] = defaultTargetPaths?.[kind] ?? '';
    }
    return init;
  });
  const [showPaths, setShowPaths] = useState(
    Object.values(defaultTargetPaths ?? {}).some((v) => v.length > 0),
  );

  const setPath = (kind: string, value: string) =>
    setPaths((prev) => ({ ...prev, [kind]: value }));

  return (
    <main className="scan-form">
      <h1>homelinter</h1>
      <p className="subtitle">Duplicate and home cleanup suggestions for your files</p>
      {hasSuggestions && (
        <div className="resume">
          <span>An existing scan session has suggestions ready.</span>
          <button type="button" onClick={onResume}>open suggestions</button>
        </div>
      )}
      <form
        onSubmit={(e) => {
          e.preventDefault();
          onStart({ root: root.trim(), hidden, targetPaths: paths });
        }}
      >
        <label className="field">
          <span>Scan root (absolute path; empty = home)</span>
          <input
            type="text"
            value={root}
            onChange={(e) => setRoot(e.target.value)}
            placeholder="/home/you"
          />
        </label>
        <label className="field check">
          <input
            type="checkbox"
            checked={hidden}
            onChange={(e) => setHidden(e.target.checked)}
          />
          <span>Include hidden files and directories</span>
        </label>
        <details
          className="scan-paths"
          open={showPaths}
          onToggle={(e) => setShowPaths((e.target as HTMLDetailsElement).open)}
        >
          <summary>Per-kind target path overrides (optional)</summary>
          <p className="hint">
            Destination format string per asset kind, relative to home or absolute.
            Must contain a placeholder; empty uses the configured default.
          </p>
          <div className="field-grid">
            {KINDS.map(([kind, def]) => (
              <label className="field" key={kind}>
                <span>{kind}</span>
                <input
                  type="text"
                  value={paths[kind]}
                  placeholder={def}
                  onChange={(e) => setPath(kind, e.target.value)}
                />
              </label>
            ))}
          </div>
        </details>
        <div className="toolbar">
          <button
            type="submit"
            disabled={startDisabled}
            title={startDisabled ? 'a scan or execution is already in progress' : undefined}
          >
            scan
          </button>
        </div>
      </form>
    </main>
  );
}
