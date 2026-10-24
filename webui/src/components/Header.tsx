import { stateLabel } from '../lib/format';
import type { Theme } from '../types';

interface HeaderProps {
  state: string;
  root: string | null;
  busy: boolean;
  theme: Theme;
  scanDisabled?: boolean;
  onNewScan: () => void;
  onConfig: () => void;
  onToggleTheme: () => void;
}

function ThemeIcon({ theme }: { theme: Theme }) {
  if (theme === 'dark') {
    return (
      <svg viewBox="0 0 24 24" aria-hidden="true" focusable="false">
        <circle cx="12" cy="12" r="4" />
        <path d="M12 2v2M12 20v2M4.93 4.93l1.42 1.42M17.66 17.66l1.41 1.41M2 12h2M20 12h2M4.93 19.07l1.42-1.42M17.66 6.34l1.41-1.41" />
      </svg>
    );
  }
  return (
    <svg viewBox="0 0 24 24" aria-hidden="true" focusable="false">
      <path d="M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8z" />
    </svg>
  );
}

export function Header(props: HeaderProps) {
  const {
    state,
    root,
    busy,
    theme,
    scanDisabled,
    onNewScan,
    onConfig,
    onToggleTheme,
  } = props;
  const badgeClass = state === 'ready' || state === 'done' ? 'state-ok' : state === 'failed' ? 'state-bad' : 'state-busy';
  const nextTheme = theme === 'dark' ? 'light' : 'dark';
  return (
    <header className="header">
      <span className="title">homelinter</span>
      <span className={badgeClass}>{stateLabel(state)}{busy ? '...' : ''}</span>
      <span className="root-label"><span className="root-label-text">{root ?? 'no scan root'}</span></span>
      <nav className="header-actions">
        <button
          type="button"
          onClick={onNewScan}
          disabled={scanDisabled}
          title={
            scanDisabled ? 'a scan or execution is in progress' : undefined
          }
        >
          scan
        </button>
        <button type="button" onClick={onConfig}>config</button>
        <button
          type="button"
          className="theme-toggle"
          onClick={onToggleTheme}
          aria-label={`Switch to ${nextTheme} theme`}
          title={`Switch to ${nextTheme} theme`}
        >
          <ThemeIcon theme={theme} />
        </button>
      </nav>
    </header>
  );
}
