/**
 * ShortCard — a deterministic 1080x1920 vertical branded title/body card.
 *
 * Driven entirely by inputProps; no network, no fonts beyond the system
 * sans stack, no randomness — so renders are reproducible frame-for-frame.
 * A subtle spring-driven entrance and a thin progress bar are the only
 * motion; everything is a pure function of `frame`.
 */
import React from 'react';
import {
  AbsoluteFill,
  interpolate,
  spring,
  useCurrentFrame,
  useVideoConfig,
} from 'remotion';

export type ShortCardProps = {
  title: string;
  body: string;
  accent?: string;
  durationSec: number;
};

export const SHORT_CARD_DEFAULTS: ShortCardProps = {
  title: 'AugmentAgent',
  body: 'A branded vertical card rendered from JSON props.',
  accent: '#5B8DEF',
  durationSec: 20,
};

const FONT_STACK =
  '-apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif';

export const ShortCard: React.FC<ShortCardProps> = ({
  title,
  body,
  accent,
  durationSec,
}) => {
  const frame = useCurrentFrame();
  const { fps, durationInFrames, width, height } = useVideoConfig();
  const accentColor = accent && accent.length > 0 ? accent : '#5B8DEF';

  // Entrance: title rises + fades over the first ~0.7s, body trails it.
  const titleSpring = spring({
    frame,
    fps,
    config: { damping: 200 },
    durationInFrames: Math.round(fps * 0.7),
  });
  const bodySpring = spring({
    frame: Math.max(0, frame - Math.round(fps * 0.25)),
    fps,
    config: { damping: 200 },
    durationInFrames: Math.round(fps * 0.7),
  });

  const titleY = interpolate(titleSpring, [0, 1], [40, 0]);
  const bodyY = interpolate(bodySpring, [0, 1], [32, 0]);

  // Thin progress bar across the very bottom — a pure function of frame.
  const progress = interpolate(frame, [0, Math.max(1, durationInFrames - 1)], [0, 1], {
    extrapolateLeft: 'clamp',
    extrapolateRight: 'clamp',
  });

  return (
    <AbsoluteFill
      style={{
        backgroundColor: '#0B0E14',
        fontFamily: FONT_STACK,
        color: '#F5F7FA',
      }}
    >
      {/* Soft accent glow anchored top-left, deterministic. */}
      <div
        style={{
          position: 'absolute',
          top: -260,
          left: -260,
          width: 720,
          height: 720,
          borderRadius: '50%',
          background: `radial-gradient(circle, ${accentColor}33 0%, transparent 70%)`,
        }}
      />

      <AbsoluteFill
        style={{
          padding: 120,
          justifyContent: 'center',
          alignItems: 'flex-start',
        }}
      >
        {/* Accent rule above the title. */}
        <div
          style={{
            width: 140,
            height: 10,
            borderRadius: 5,
            backgroundColor: accentColor,
            marginBottom: 48,
            opacity: titleSpring,
          }}
        />

        <div
          style={{
            fontSize: 96,
            fontWeight: 800,
            lineHeight: 1.08,
            letterSpacing: -1.5,
            opacity: titleSpring,
            transform: `translateY(${titleY}px)`,
          }}
        >
          {title}
        </div>

        <div
          style={{
            marginTop: 56,
            fontSize: 52,
            fontWeight: 400,
            lineHeight: 1.4,
            color: '#C3CAD6',
            maxWidth: width - 240,
            opacity: bodySpring,
            transform: `translateY(${bodyY}px)`,
          }}
        >
          {body}
        </div>
      </AbsoluteFill>

      {/* Footer wordmark. */}
      <div
        style={{
          position: 'absolute',
          bottom: 96,
          left: 120,
          fontSize: 34,
          fontWeight: 600,
          letterSpacing: 4,
          textTransform: 'uppercase',
          color: accentColor,
          opacity: 0.85,
        }}
      >
        AugmentAgent
      </div>

      {/* Bottom progress bar. */}
      <div
        style={{
          position: 'absolute',
          bottom: 0,
          left: 0,
          width: width * progress,
          height: 8,
          backgroundColor: accentColor,
        }}
      />

      {/* Suppress unused-var lint for height in strict mode. */}
      {height < 0 ? <span /> : null}
    </AbsoluteFill>
  );
};
