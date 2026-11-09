/** Which side of the line something ended up on. Unknown makes no claim. */
export function WhereMark({
  place,
  small = false,
}: {
  place: 'local' | 'cloud' | undefined;
  small?: boolean;
}) {
  return (
    <span className={`where-mark ${place ?? 'unknown'} ${small ? 'small' : ''}`} aria-hidden="true">
      <i />
    </span>
  );
}

/** What each side of the rule means, at the top of the tape. */
export function TapeLegend({
  left = 'Them',
  right = 'You',
  lost,
}: {
  left?: string;
  right?: string;
  /** The channel whose device has gone. */
  lost?: string;
}) {
  return (
    <div className={`tape-legend ${lost ? `lost-${lost}` : ''}`} aria-hidden="true">
      <span className="them">{left}</span>
      <span className="you">{right}</span>
    </div>
  );
}

/** The You/Them model, shown in the transcript's own form rather than described. */
export function TwoVoices() {
  return (
    <div className="exchange">
      <p className="exchange-line them">
        <span className="voice-label">Them</span>
        Everything your computer plays is written down here.
      </p>
      <p className="exchange-line you">
        <span className="voice-label">You</span>
        And whatever you say into your microphone, here.
      </p>
    </div>
  );
}
