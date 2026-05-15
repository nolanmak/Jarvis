/**
 * Remotion entrypoint. `@remotion/bundler` bundles this module; it must
 * call `registerRoot` exactly once.
 */
import { registerRoot } from 'remotion';
import { RemotionRoot } from './Root';

registerRoot(RemotionRoot);
