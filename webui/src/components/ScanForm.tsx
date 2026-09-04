import { useState } from 'react';

export interface ScanFormValues {
  root: string;
  hidden: boolean;
  targetPaths: string;
}

interface ScanFormProps {
  defaultRoot: string;
  hasSuggestions: boolean;
  onStart: (values: ScanFormValues) => void;
  onResume: () => void;
}

export function ScanForm(props: ScanFormProps) {
  const { defaultRoot, hasSuggestions, onStart, onResume } = props;
  const [root, setRoot] = useState(defaultRoot);
  const [hidden, setHidden] = useState(false);
  const [targets, setTargets] = useState('');

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
          onStart({ root: root.trim(), hidden, targetPaths: targets });
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
        <label className="field">
          <span>Target path overrides (optional, kind=template per line)</span>
          <textarea
            value={targets}
            onChange={(e) => setTargets(e.target.value)}
            placeholder="photo={year}/{month}/{name}"
            rows={3}
          />
        </label>
        <div className="toolbar">
          <button type="submit">scan</button>
        </div>
      </form>
    </main>
  );
}