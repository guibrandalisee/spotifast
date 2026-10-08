Offline decoder fixtures are synthetic 440 Hz tones, stereo at 44.1 kHz,
lasting 0.5 seconds. They contain no Spotify or third-party recording.

The Ogg and MP3 fixtures are encrypted in memory by regression tests. Ogg
receives a synthetic Spotify-style header; MP3 has no custom header.

MP3 generation: `ffmpeg -f lavfi -i sine=frequency=440:duration=0.5 -ac 2
-ar 44100 -b:a 160k offline-tone.mp3`.
