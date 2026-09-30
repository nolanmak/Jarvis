# Prerecorded speech fixtures

These 20 unmodified one-word WAV clips come from Google’s Speech Commands mini dataset,
distributed by TensorFlow under a [CC BY license](https://www.tensorflow.org/tutorials/audio/simple_audio).
Source archive: <https://storage.googleapis.com/download.tensorflow.org/data/mini_speech_commands.zip>.
Original archive SHA-256: `49650f2341b26d886b46b3f4fb8fed59e30300b17550f1ee4a768b3106cf93a0`.
The folder name is each clip’s reference word. The test lowercases and trims the provider event before comparison.
`silence.wav` is generated in this repository as one second of zero-valued 16 kHz mono PCM; it is not part of the Google dataset.

The deterministic provider server returns the reference word after it receives decoded/resampled PCM. This tests transport, commit semantics, and one-turn routing, not speech recognition accuracy. Live vendor results must be recorded separately.

| Reference word | Original file | SHA-256 |
| --- | --- | --- |
| down | `004ae714_nohash_0.wav` | `ea1a414f185f6476b94797bdc682da7c964ba3b2ddfc8ebf80d76cbe18d936a4` |
| down | `00b01445_nohash_1.wav` | `2e018e932d23ce0d1eefe959aa45a4f74870eb9602e429b4d65f8b51d8d91119` |
| down | `00f0204f_nohash_0.wav` | `c921fe62586ceb6a2422dc04ffe5637e3559751054d9ace43367863ae0cf1ede` |
| go | `004ae714_nohash_0.wav` | `91bd3ade5657b3b2e27f7dad1cbb2749916042bfbb849c2d4f9995bf2a4c7a76` |
| go | `0132a06d_nohash_2.wav` | `bb62a519e337967e98ee065cf1f794c1f07480863b34d726444654a5c8ea317c` |
| left | `00b01445_nohash_0.wav` | `f04aa076c7ab26efe74ea21d4fb6ccc8073c360d668afb55b9bf9dc9c3c3c2c1` |
| left | `012c8314_nohash_0.wav` | `667914a68b1a131d6efc476d99e5929de1b457317ee40858787b858c50f4a87f` |
| no | `012c8314_nohash_0.wav` | `5d7193eb0fe19dccc8ae0cdf4a15280c8b94c5465ee3e3e55b0aec72f76b15c3` |
| no | `0132a06d_nohash_1.wav` | `00d4f781bed7395e36febb0e6d4a304a50ab8ed168ea46c7252b37ec6a90f1a4` |
| no | `0132a06d_nohash_3.wav` | `694cf68a40a88a2dbd60058eacf804c12ca8af7d3206721110718df306bc68c9` |
| right | `012c8314_nohash_1.wav` | `b98e234ee361621b24b4467dbf68b4a26865d0d3049a70b763eb460884eca7d5` |
| right | `0132a06d_nohash_1.wav` | `4868c9b9332ec5ea099261a9543c04d92936e0f16d5c9eded84ed993cdfd4d62` |
| stop | `012c8314_nohash_0.wav` | `3b0804b7e5ecf23816f81ee4cc4c243e608df36cc572ca49fb9bd10f926bacd7` |
| stop | `0132a06d_nohash_3.wav` | `52f0c7085f65b1313f6620659d801e21e1ca8524d01498b8d8018e7a0e5c28cb` |
| up | `0132a06d_nohash_2.wav` | `ad7524a2d2f208e9f16e926b52b194e4e3033ee5e7c59c2f34deb062f665c2a9` |
| up | `0135f3f2_nohash_1.wav` | `30d9c7e4adadd9425840723a262e80b233d5a232e20d5e2c7d92d3d7fea1c0de` |
| up | `0137b3f4_nohash_0.wav` | `4c006c40f47246ee7f86f5afe6042f57e7500777e82c353f7d4f083b2535c61c` |
| yes | `004ae714_nohash_0.wav` | `2e38228747f53aab91ed5fdd0d9be74f834cf43f136e59c4de33c428bcb98fba` |
| yes | `00f0204f_nohash_0.wav` | `4c619b0c00e0e281df3c3981db329249cdc42bd94a892e41a81949b10bf83229` |
| yes | `012c8314_nohash_0.wav` | `6e382b991376beece4c26b8d934b2ad7f1bd6ea9aca2b6439061f1447dd9757a` |
