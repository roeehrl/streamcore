# Example data

`pattern-640x360.h264`: the clip `video_host` loops. 60 frames of FFmpeg's `testsrc2` pattern,
640×360, H.264 Constrained Baseline, Annex B, an IDR every 30 frames (with SPS and PPS before
each), one slice per picture, no B-frames, BT.709 limited range stated in the VUI, about 94 KB.
Generated with FFmpeg and libx264:

```sh
ffmpeg -f lavfi -i testsrc2=size=640x360:rate=60 -frames:v 60 \
  -c:v libx264 -threads 1 -profile:v baseline -preset veryfast -tune zerolatency \
  -g 30 -keyint_min 30 -sc_threshold 0 -bf 0 -b:v 800k -maxrate 800k -bufsize 400k \
  -pix_fmt yuv420p \
  -x264-params repeat-headers=1:colorprim=bt709:transfer=bt709:colormatrix=bt709:range=tv \
  -bsf:v filter_units=remove_types=6 -f h264 pattern-640x360.h264
```

`-threads 1` keeps one slice per picture (the host splits access units at slices);
`filter_units=remove_types=6` drops the SEI that names the encoder build.
