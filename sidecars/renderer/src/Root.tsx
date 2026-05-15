/**
 * Remotion root: registers the single parametrized ShortCard composition.
 *
 * Duration is derived from the `durationSec` input prop via
 * `calculateMetadata` so the Rust client controls clip length without the
 * sidecar re-bundling. fps is fixed at 30; canvas is 1080x1920 vertical.
 */
import React from 'react';
import { Composition } from 'remotion';
import { ShortCard, SHORT_CARD_DEFAULTS, ShortCardProps } from './ShortCard';

const FPS = 30;

export const RemotionRoot: React.FC = () => {
  return (
    <Composition
      id="ShortCard"
      component={ShortCard}
      durationInFrames={SHORT_CARD_DEFAULTS.durationSec * FPS}
      fps={FPS}
      width={1080}
      height={1920}
      defaultProps={SHORT_CARD_DEFAULTS}
      calculateMetadata={({ props }: { props: ShortCardProps }) => {
        const seconds =
          typeof props.durationSec === 'number' && props.durationSec > 0
            ? props.durationSec
            : SHORT_CARD_DEFAULTS.durationSec;
        return {
          durationInFrames: Math.max(1, Math.round(seconds * FPS)),
          fps: FPS,
        };
      }}
    />
  );
};
