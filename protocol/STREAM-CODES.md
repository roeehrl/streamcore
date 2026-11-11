# Stream wire codes

The error codes a host reports for a video stream, in a consumer's control protocol (a stream
command's rejection, a stream failure). The strings are the contract: they are ported unchanged
from the reference host and defined in `crates/streamcore-video/src/error.rs` (`codes`), which a
test checks against this file. The only code new in v1 is `session.stale`.

| Group | Code | Meaning |
| --- | --- | --- |
| Commands | `stream.config` | Invalid settings: dimensions, frame rate, quality, rate, region or plan out of range. |
| | `stream.limit` | A stream limit was reached. |
| | `stream.busy` | Another command or native operation is pending, or a capture whose stop failed or is unknown blocks a replacement. |
| | `stream.cancelled` | Screen authority was revoked or the stream closed while the command ran. |
| | `stream.timeout` | A command was not answered in time. |
| | `stream.command` | The command is not valid for this stream. |
| | `stream.missing` | No such stream, or no capture to update. |
| Mode | `stream.viewOnly` | Fit or scale was requested on a View stream. |
| | `stream.viewOnlyUnavailable` | A View source is not on screen or has no visible window. |
| | `stream.ownWindow` | The host app's own windows can be viewed but not controlled. |
| Sources | `stream.target` | The capture target is invalid. |
| | `stream.dimensions` | The source's dimensions are unusable. |
| | `stream.content` | The source is gone or its content is unavailable. |
| | `stream.viewport` | The requested viewport is invalid. |
| | `stream.fit` | Window fit failed. |
| | `stream.scale` | Window scaling failed. |
| | `stream.restore` | Restoring a fitted window failed. |
| Capture and encode | `stream.start` | The capture backend failed to start. |
| | `stream.stop` | The capture backend failed to stop. |
| | `stream.update` | A live capture or rate update failed. |
| | `stream.capture` | The capture could not be opened. |
| | `stream.output` | Converting or composing a frame failed. |
| | `stream.encoder` | The encoder could not be created or re-created, or failed too often. |
| | `stream.encode` | Encoding a frame failed unrecoverably. |
| | `stream.sequence` | The stream's sequence or generation space is exhausted. |
| Canvas | `stream.firstFrame` | A canvas member produced no first image in time. |
| | `stream.memberUnavailable` | A canvas member is unavailable. |
| | `stream.layoutChanged` | The display layout changed under a canvas. |
| Sessions | `session.stale` | A resume named a generation that is no longer current (STREAMCORE-VIDEO.md 8.7). |

Permission codes are `permission.*` ([input model](../docs/INPUT-MODEL.md)).
