# Road Viewer for Home Assistant

[한국어](README.md) · English

> **This repository is Road Viewer 2.x, rebuilt in Rust.** The current version is **2.1.1**.
>
> It succeeds the Python [Road Viewer 1.x](https://github.com/Qualified4/roadviewer-ha), whose repository no longer receives new features. The Home Assistant app store and sidebar show it as **Road Viewer**. It is a separate app with a different slug, so the 1.x app is not updated to it automatically. To move from 1.x, see [cutover and rollback](docs/hars/CUTOVER.md) (Korean). Unverified environments and remaining risks are listed in the [implementation and validation status](docs/hars/STATUS.md) (Korean).
>
> The features below describe the current Road Viewer 2.x. Its web UI is based on Python Road Viewer 1.x, and intentional differences from 1.x are listed in the [implementation and validation status](docs/hars/STATUS.md#python과-의도적으로-다른-점) (Korean).
>
> [Implementation and validation status](docs/hars/STATUS.md) · [Side-by-side install, cutover and rollback](docs/hars/CUTOVER.md) · [Real HAOS validation](docs/hars/HAOS-VALIDATION.md) (Korean)

Upload openpilot driving logs and front/wide camera recordings to **replay video, road views and vehicle information graphs on the same timeline inside Home Assistant**.

**Camera overlay and vehicle graphs**

Compare lanes, vehicle boxes and blind-spot indications over the front video with vehicle graphs at the same instant. Seeking moves the video and graphs together.

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/roadviewer-overview-dark.png"><img src="docs/images/roadviewer-overview.png" alt="Road Viewer showing high-quality video with lanes, vehicle boxes, blind-spot indications and synchronized vehicle graphs"></picture>

**Camera overlay and road view**

Switch the right panel to the road view to compare a top-down view of lanes, paths and radar detections with the video. The tables below expose individual radarState and liveTracks values. These screenshots demonstrate the two panel choices; graph selection and display items are configurable.

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/roadviewer-driving-view-dark.png"><img src="docs/images/roadviewer-driving-view.png" alt="Road Viewer showing lane, vehicle, turn-signal and blind-spot overlays alongside the road view and radar detections"></picture>

## Getting started

1. Add `https://github.com/Qualified4/roadviewer-hars` under Home Assistant **Settings → Apps → App store → ⋮ → Repositories**. Some versions call apps “add-ons.”
2. Install the entry shown as **Road Viewer** in the app store, start it and open its web UI. Automatic start is on by default (`boot: auto`).
3. On the **업로드** (Upload) tab, select files with **파일 추가** (Add files) or drag them into the upload area, then press **업로드** (Upload). Uploads keep running when you switch tabs; the header shows progress.
4. When conversion finishes, press **재생** (Play) on the **로그** (Logs) tab. If automatic conversion is disabled, first press **변환** (Convert).

The screen is split into **로그 · 업로드 · 저장공간 · 장치 · 설정** (Logs, Upload, Storage, Devices, Settings) tabs, shown as a bottom tab bar on phones. **화면 모드** (display mode) on the Settings tab chooses Auto, Light or Dark; the replay video area always stays dark.

This is an **app-store repository, not a HACS integration**. The web UI uses Home Assistant Ingress, including your existing remote Home Assistant address; no separate external port is needed. Home Assistant Container has no Supervisor app store.

## Platforms and installation

| Home Assistant architecture | Image platform | Devices |
|---|---|---|
| `amd64` | `linux/amd64` | 64-bit Intel/AMD PCs, mini PCs and servers |
| `aarch64` | `linux/arm64` | 64-bit ARM Home Assistant devices |

Installation downloads **only the matching prebuilt GHCR image** (`ghcr.io/qualified4/roadviewer-hars`), not every published architecture. ARM 32-bit and i386 are unsupported. Prebuilt images avoid compiling the application on your device. Home Assistant may show download/extraction progress; its presentation depends on Supervisor and is not an exact percentage of total installation time. The image holds no FFmpeg, so the amd64 image is smaller than before FFmpeg was removed (local measurement: unpacked layers 580.9 MB → 120.6 MB, registry gzip 217.5 MB → 46.3 MB; [FFmpeg removal](docs/hars/FFMPEG-REMOVAL.md#이미지-전후-amd64-로컬), Korean).

## Supported uploads

**Local files and direct device uploads**

The **업로드** tab accepts selected or dropped files; the **장치** (Devices) tab configures HTTPS and its port and pairs or manages devices. Ports and device identifiers are masked in this screenshot. Browser file uploads do not require enabling external device access.

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/roadviewer-library-upload-dark.png"><img src="docs/images/roadviewer-library-upload.png" alt="Upload tab file selection — port and device identifiers are masked"></picture>

The log is required; include whichever cameras you need from the same segment.

| File | Purpose |
|---|---|
| `rlog.zst` | Driving log |
| `qcamera.ts` | Low-bandwidth front video |
| `fcamera.hevc` | High-quality front video |
| `ecamera.hevc` | Wide video |
| `camera.mp4` / `qcamera.mp4` | Low-resolution H.264 MP4 for re-upload |
| `fcamera.mp4` / `ecamera.mp4` | High-quality front / wide HEVC MP4 for re-upload |

For one segment, bare filenames work. For multiple segments, retain their identifiers:

```text
00000395--0d0eda17c5--7--rlog.zst
00000395--0d0eda17c5--7--qcamera.ts
00000395--0d0eda17c5--7--fcamera.hevc
00000395--0d0eda17c5--7--ecamera.hevc
00000395--0d0eda17c5--8--rlog.zst
00000395--0d0eda17c5--8--qcamera.ts
```

- Logs alone support road views and vehicle graphs. Video-only registration is unsupported.
- Downloaded camera MP4s can be re-uploaded with the same segment’s log. Segment/camera names and codecs must match. Select one format per camera rather than its original and MP4 together; existing retained videos are not overwritten.
- Add a camera later by uploading it with the identical log; it attaches to the existing recording. When adding a camera to a segment moved to Google Drive, the app checks the Drive connection before registering the upload.
- Re-uploading the same source video checks its saved hash and preserves the retained MP4 and replay data without storing another original copy. Use the download modal for original-format files.
- Compressed-log SHA-256 identifies duplicates at registration, after transfer.
- Browser uploads default to **512 MiB per batch**, configurable through `max_upload_mb` from 16–2048. This is separate from the storage budget. Device uploads support 8 GiB per batch, 50 segments, 200 files and 2 GiB per video; each log must fit `max_upload_mb`.

## Connecting a comma device

Use the [Device API documentation](docs/device-api.en.md) to build a client that connects an external device to Road Viewer and uploads logs and videos directly. It covers HTTPS connections, pairing and authentication, file uploads, and resuming interrupted transfers.

For direct device uploads, enable HTTPS on the **장치** (Devices) tab, save a port and apply it with **지금 재시작** (Restart now). A valid certificate under `/ssl` is required. External access defaults to off and is not needed for the Ingress UI.

Pairing supports code copy, remaining time and reissue. Codes last five minutes and work once; a replacement invalidates the old code. The API uses HMAC authentication and resumable chunk uploads. A compatible Carrot Web uploader can include optional high-quality front/wide files alongside the log/qcamera, skipping missing cameras. Video conversion runs on Road Viewer, not the driving device.

**Pairing a comma device**

**Add device** opens this dialog. Copy the highlighted one-time code into the device connection screen and check its remaining lifetime and pairing status. The pictured code is illustrative; generate a new code for an actual connection. Unlike the HTTPS port settings, this dialog authenticates and registers a device.

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/roadviewer-device-pairing-dark.png"><img src="docs/images/roadviewer-device-pairing.png" alt="Road Viewer one-time pairing code dialog for a comma device" width="420"></picture>

## AI agent access (MCP)

This section describes Python 1.x; see the [implementation and validation status](docs/hars/STATUS.md) (Korean) for differences in HARS 2.x.

[roadviewer-mcp](https://github.com/Qualified4/roadviewer-mcp) is a local MCP server that lets clients such as Codex or Claude Code work with Road Viewer recordings. It signs in through the existing Home Assistant Ingress flow with a long-lived token and calls only the add-on's existing API, so nothing changes in the add-on and no port is opened or exposed. It supports searching and inspecting logs, bookmarks and storage status, time-window queries of replay data and vehicle telemetry, and video/log downloads (saved to a local directory, path returned). Writes are limited to adding or updating bookmarks and adding or removing tags; it never deletes, converts or changes settings. See that repository's README for installation and client registration.

## Playback and analysis

| Feature | Capabilities |
|---|---|
| Synchronized playback | Video with road view or vehicle graphs, up to 4× speed |
| Layout | Automatic, side-by-side or stacked; adjustable width for phone, tablet and desktop |
| Road view | Lanes, road edges, model paths, leads and radar detections |
| Vehicle information | 24 selectable, reorderable graphs, including separate LATERAL command and restored yRel values |
| Tags | Name and color segments, filter the library, and add/remove tags in bulk |
| Map | Segments with GPS get a **지도** (Map) analysis tab showing the travelled route and the current position, plus an optional small map over the video. See [Drive route maps](#drive-route-maps). |
| Scene bookmarks | Save a log timestamp with a title/note on the **북마크** analysis tab and jump back from seek-bar markers or the list |
| Navigation and pins | The app-bar title (start time · 구간 k (i/N)) opens the segment chooser; previous/next buttons, the segment strip and per-segment pins |
| Playback bar | Floating bar with a segment strip (centred on the current segment on long drives), automatic-control/takeover track, seek bar with bookmark markers, frame step, current/total time toggle, bookmark and speed |
| Over the video | Camera/quality chooser, overlay toggle with ⚙ settings, speed, steering-wheel state with an **개입** (takeover) ring, fullscreen |

**Forward and rear road view**

Choose paired **front/rear ranges of 25/5m, 50/10m or 80/20m**. **후방 표시 · LR / RR** shows rear targets behind the ego vehicle and 0m line; switching it off restores the full forward view. Rear dashed lines are position guides rather than extrapolated lanes. Reconvert an older recording to populate LR/RR values if they are missing. Camera overlays retain BSD instead of adding rear targets.

**Wide-camera playback and overlays**

Upload `ecamera.hevc` with the log to select **와이드** (Wide) in the camera chooser over the video. This example shows lanes, road edges, the model path, a vehicle box, radar detections and a turn signal over the wider field of view. The overlay uses the wide camera's sensor and calibration; switching back to the front video preserves the log position.

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/roadviewer-wide-camera-dark.png"><img src="docs/images/roadviewer-wide-camera.png" alt="Road Viewer wide-camera video with lanes, model path, FF vehicle box, radar detections and a left turn signal overlay"></picture>

Choose a **기본 화질** (default quality) for each connection. The defaults are the small front video (qcamera) on mobile data and the high-quality front on Wi-Fi/wired. Change it with **기본 화질** beside 닫기 (close) in the camera list over the video; the choice is kept per browser. Where the browser does not report the connection type (iPhone, iPad, desktop browsers and others), phones and tablets follow the mobile-data choice and other devices the Wi-Fi/wired one. Turn on **마지막으로 고른 화질 유지** (keep the last chosen quality, off by default) in the same window to remember the current camera and any camera you pick afterward (including wide) in this browser. The remembered camera opens first when the next segment plays automatically or you open another segment or log. If that camera is missing, the app tries the connection's default quality, then the other front video, then wide. A camera selected automatically after a playback failure does not replace the remembered camera. Switching preserves log time and playing/paused state. A failing video triggers an attempt to use another available source. Wide overlays use the wide sensor/calibration; missing information produces an explanation. Older prepared logs may need reanalysis to obtain wide calibration.

In fullscreen, the buttons, minimap, speed, steering wheel and messages stay inside the picture, and the playback bar spans the screen.

Overlay settings control box height/opacity, path width, BSD wall height and individual labels. LF/FF/RF boxes reconstruct supported CCNC display commands recorded in the log. BSD, turn signals and target distance appear when the corresponding data is available. Camera overlays are independent of the road panel's 25/50/80m range; BSD walls retain a 40m limit.

Display mode, layout, display selections, graph order and collapsed sections are remembered in the current browser. Library and playback pins share the same per-segment setting.

### Current UI details

Open the ⚙ settings beside the **겹쳐 보기** overlay switch over the video to adjust individual box/BSD/TARGET labels, dimensions, and opacity. The ground-anchor and estimated-size explanation is inside this dialog.

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/roadviewer-overlay-settings-dark.png"><img src="docs/images/roadviewer-overlay-settings.png" alt="Camera overlay settings" width="530"></picture>

Lane probability and lateral y[0] distance share the same layout alongside road-edge and lead information.

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/roadviewer-boundaries-probability-dark.png"><img src="docs/images/roadviewer-boundaries-probability.png" alt="Lane probabilities and road information"></picture>

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/roadviewer-boundaries-distance-dark.png"><img src="docs/images/roadviewer-boundaries-distance.png" alt="Lateral y[0] values in the same layout"></picture>

## Drive route maps

Converting a log also stores its position once per second (about 1 KB per minute), so routes can be drawn on a map. The device GPS is used when it has a fix, else the car's own GPS sent over CAN (`carState.vehicleGps`), else carrot's estimated position. Reconvert logs without a map route to generate one.

**All drives**: the **목록 | 지도** (List | Map) switch at the top right of the Logs tab draws every drive the current search and filters show, one pastel line each. Picking a drive in the glass card list on the right fits the map to it and opens **재생 · 주행 지도 · 목록에서 보기** (Play, Drive map, Show in list) inside the card. The button at the top right folds and unfolds the list without moving the map. List cards show the drive number (for example, **주행 395**) alongside the date and time. When you select another drive, the buttons on the two cards fold and unfold smoothly, or switch immediately if reduced motion is enabled.

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/roadviewer-library-map-dark.png"><img src="docs/images/roadviewer-library-map.png" alt="Map view of the Logs tab: three example drives as pastel lines with a glass card list of drives on the right"></picture>

**Drive map**: open it from the route shape on a drive card (on a PC it unfolds into a square map on hover; on phones use the **지도** button next to Play), the segment ⋯ menu's **지도에서 보기** (Show on map), or **주행 지도** on the all-drives card. Segment cards and their part of the route highlight each other. With the list folded (on phones, with the sheet pulled down) the chosen segment appears as a pill at the bottom, ready to play. In the light theme, gold edges highlight the selected card on both the drive map and the all-drives map.

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/roadviewer-drive-map-dark.png"><img src="docs/images/roadviewer-drive-map.png" alt="Drive map: one drive drawn per segment, glass cards for segments 0–3 on the right, segment 1 highlighted"></picture>

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/roadviewer-drive-map-mobile-dark.png"><img src="docs/images/roadviewer-drive-map-mobile.png" alt="Drive map on a phone: the map on top and a resizable sheet of segment cards below" width="320"></picture>

**Maps while replaying**: the **지도** (Map) analysis tab draws the travelled part solid and the rest faded, with a car marker that follows playback; tapping the route seeks there. Overlay settings › Map › **영상 위 작은 지도** (small map over the video) puts a small map in the video's top-right corner that always keeps the car a little below the middle, in automatic, light or dark colours; its own **글자 숨기기** (hide labels) and **회색으로** (grey) options work like the two switches of the map display popover, and dragging its bottom-left corner resizes it.

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/roadviewer-replay-map-dark.png"><img src="docs/images/roadviewer-replay-map.png" alt="Replay screen with the Map tab showing the travelled route and the car's current position"></picture>

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/roadviewer-video-minimap-dark.png"><img src="docs/images/roadviewer-video-minimap.png" alt="Small map over the driving video, under the speed and steering wheel readouts"></picture>

The basemap is [OpenFreeMap](https://openfreemap.org)'s vector map built from OpenStreetMap data; no API key is needed. The layers button under the zoom buttons opens a **지도 표시** (map display) popover with two independent switches: **글자 숨기기** (hide labels) removes place and road names, place icons, road shields and one-way arrows while keeping the map's colours (such as the yellow main roads in light mode); **회색 지도** (grey map) switches to a calm grey style without 3D buildings, place icons, shields and arrows, keeping the names. With both on, only the drive lines stand out. The viewing browser fetches map tiles directly from OpenFreeMap, so requests for the area you look at go there. Positions are used only for the maps in the app and are never added to downloads, exports, the device API or archives. The routes in the screenshots are example routes along roads between public landmarks. See the [user guide](roadviewer_hars/DOCS.en.md#drive-route-maps) for details.

## Organize segments with tags

Tags **classify a whole segment**. Use labels such as “Lane change”, “Rain” or “Needs review” to describe driving conditions and follow-up work, then choose a tag filter chip on the Logs tab to narrow the library. Colored badges appear next to the recording name/pin and are also visible during playback.

The following tag and bookmark screenshots use **synthetic examples in the actual Korean UI**, without personal driving logs.

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/roadviewer-tags-library-dark.png"><img src="docs/images/roadviewer-tags-library.png" alt="Saved example segments with colored lane-change, review and rainy-weather tags, plus the tag filter chips"></picture>

1. Choose **태그 편집** (Edit tags) in a segment's ⋯ menu (**전체 구간 태그 편집** in a route card's ⋯ menu) or **태그** (Tags) in playback.
2. Enter a name, select a color, then choose **추가 → 저장** (Add → Save). Select an existing tag without raising the mobile keyboard, or click a badge in the editor to change its name/color.
3. To tag several segments together, press **선택** (Select) on the Logs tab (or long-press a segment on touch screens), select the segments and use **태그** in the bottom action bar.

Tags support 12 colors, names up to 32 characters and up to 100 tags per segment. Saving tags does not reconvert logs or videos.

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/roadviewer-tags-editor-dark.png"><img src="docs/images/roadviewer-tags-editor.png" alt="Actual tag editor with lane-change and review tags, a new merge tag, and the twelve-color palette" width="556"></picture>

## Return to a moment with scene bookmarks

A bookmark stores **a specific log timestamp within one segment**. Mark a lane change, a lead-distance check or the start of deceleration to return directly to that moment later.

1. Play or seek to the desired time, then click **북마크** (Bookmark) in the floating playback bar. A recording popup captures that time without switching the current analysis view.
2. Check the captured timestamp and choose **저장** (Save). The title and note are optional; an empty title defaults to the scene time. **현재 시각으로** (Use current time) updates the editor’s timestamp to the current playback position.
3. Click a saved **time/title** in the bookmark tab or a **seek-bar marker** to move the video, road view and graphs together, preserving the playing/paused state.

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/roadviewer-bookmarks-editor-dark.png"><img src="docs/images/roadviewer-bookmarks-editor.png" alt="Recording popup opened with the Bookmark button: the captured time with a title and note being entered" width="460"></picture>

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/roadviewer-bookmarks-list-dark.png"><img src="docs/images/roadviewer-bookmarks-list.png" alt="Bookmark analysis tab listing saved merge, driver-steering and lead-braking scenes in time order" width="460"></picture>

Saved bookmarks also appear as small timeline markers. Hover over a marker to see its time and title.

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/roadviewer-bookmarks-timeline-dark.png"><img src="docs/images/roadviewer-bookmarks-timeline.png" alt="Playback bar with bookmark markers at 8.5, 22 and 41.2 seconds and the Bookmark button"></picture>

Bookmarks use log time rather than a camera file’s timestamp, so changing cameras still leads to the same moment. Each segment supports 100 bookmarks, with titles up to 80 characters and notes up to 1000 characters; entries can be edited or deleted in the bookmark tab. Tags and bookmarks survive reconversion and removal of prepared data, but are removed when the segment is **completely deleted**.

**Tags and bookmarks do not protect recordings from automatic deletion.** Pin a segment separately when it needs to be retained.

## Conversion and file management

**Conversion behavior and storage policy**

Set automatic conversion, concurrent jobs and display mode on the **설정** (Settings) tab. The **저장공간** (Storage) tab controls the budget and full-storage policy. Actual concurrency depends on the configured maximum and available memory. Once an item is prepared, it shows **메모리 대기 중** (Waiting for memory) while waiting for memory.

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/roadviewer-storage-overview-dark.png"><img src="docs/images/roadviewer-storage-overview.png" alt="Local usage and Google Drive connection status"></picture>

Newly completed conversions remux qcamera H.264 and front/wide HEVC into MP4 without re-encoding. Verification compares SHA-256 hashes of compressed pictures and headers, packet counts and timestamps without decoding every frame; it does not screen the source for every possible decoding error. Originals are removed only after compressed-picture, decoder-header, packet-count and log-clock verification; failed sources and rlog.zst are preserved. With automatic conversion off, uploads remain in their uploaded form until manual conversion. HEVC playback requires browser/device support; playback failure switches to the low-resolution camera when available.

The bar stays visible from preparation until the large compressed files have been saved. With video, stage weights are preparation/decompression 0–4%, log reading 4–45%, frame analysis 45–60%, camera reading/remuxing/verification 60–80%, and graph generation, JSON encoding, compression/writes and replay compaction 80–100%. Log-only recordings redistribute the video share across reading, analysis and saving. Updates use consumed bytes, processed frames/samples and written bytes, with smooth transitions at roughly one-second intervals. Labels name the actual operation and show overall progress. Atomic completion of the large writes reaches 100%; the brief summary/registration phase hides the bar and keeps only the sweep effect. Heavy conversion work is completed before that final phase. These are work-based stage weights, not a remaining-time estimate.

| Action | Effect |
|---|---|
| Remove prepared data | Preserve original logs/videos and each camera's sole MP4; remove analysis/graphs and reproducible MP4s. Cancel queued work; reject removal while processing |
| Completely delete | Delete all originals and prepared files for the segment |
| Clean storage now | Preserve registered recordings and clean incomplete browser-upload temporary files |

**Segment status and file management**

The **로그** tab has a **목록 | 지도** (List | Map) switch at the top right ([Drive route maps](#drive-route-maps)) and narrows segments with search and filter chips (All, Pinned, Unconverted, Processing, Drive, NAS and one per tag) and groups each route's segments into a **route card** under date headings. A card shows a thumbnail and total length, the route shape when it has GPS, the start time (`HH:MM 출발`, or `업로드 HH:MM` when unknown), a segment status bar and **자동 제어 N% · 개입 N회** (automatic control ratio and takeover count). Cards start folded each time you open the list. Each segment row shows processing status, video availability and retained/prepared file sizes, and chips under each segment show where the log and each camera video (high-quality, low-resolution, wide) are kept: **로컬** (local), **Drive** or **NAS**. The route card's **Convert** button queues all eligible segments, including ones hidden by search or filters and those in a collapsed card. An expanded segment row's **Convert** button queues that segment alone. Ready segments offer **Play**. Each segment's ⋯ menu offers remove, download, edit tags, copy log name, pin and complete deletion.

Thumbnails and start times are generated during conversion. If a thumbnail cannot be made, successfully converted video and drive analysis are still available. The control ratio and takeover count are computed from telemetry.

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/roadviewer-library-logs-dark.png"><img src="docs/images/roadviewer-library-logs.png" alt="Saved recordings with per-segment playback, pinning and management controls"></picture>

**Download logs and camera videos**

Open **다운로드** (Download) from a segment’s ⋯ menu or the replay ⋯ menu. Choose the format and included files, then download individual files or a ZIP. On the replay screen, the app bar’s ⋯ menu offers the same **제거** (Remove), **다운로드** (Download) and **완전 삭제** (Completely delete) as the library; confirming a removal or deletion returns to the library.

- **MP4:** Download the retained camera video as stored. HEVC front/wide playback requires codec support in the browser/device.
- **Original format:** Remux f/e to HEVC and q to TS without re-encoding. Quality is preserved, but the bytes and hashes may differ from the original upload.
- **Select all / Clear selection:** Choose the ZIP contents. **받기** (Get) beside a file downloads it individually without ZIP.
- **Several segments:** Selection mode uses the same popup for up to 150 selected segments. Expand **개별 받기** (Individual downloads) to choose a segment’s file, or download the selected files as a ZIP. Missing cameras are omitted.

For several segments, press **선택** (Select) on the Logs tab, or long-press a segment on touch screens, to enter the in-list **selection mode**. Route-card checkboxes select a whole route and segment checkboxes single segments; a minus marks a partial selection. The bottom action bar offers **태그**, **다운로드**, **변환**, **변환 데이터 제거**, **완전 삭제**, **고정 포함** (Include pinned) and **취소** (Cancel).

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/roadviewer-download-options-dark.png"><img src="docs/images/roadviewer-download-options.png" alt="Download popup selecting logs and front, wide and low-resolution videos in MP4/original format, with individual-file and ZIP controls" width="490"></picture>

ZIPs stream without retaining a download copy on the server. Each camera’s sole MP4 is a retained video, so removing prepared data preserves it.

Selection mode handles bulk conversion/removal/deletion. Pinned segments are excluded from bulk removal/deletion by default. **고정 포함** explicitly includes them for that selection session; starting selection mode again restores protection. Individual ⋯ menus allow explicit deletion of pinned recordings.

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/roadviewer-bulk-selection-dark.png"><img src="docs/images/roadviewer-bulk-selection.png" alt="Route and segment selection in selection mode with the bottom action bar" width="530"></picture>

The **저장공간** tab follows four steps: status and connections, archiving basics, execution/advanced settings, and a live behavior summary. Choose presets or custom values, review the summary, and save together. Manual archiving is disabled while settings are unsaved.

**Google Drive MOVE is extension storage for local data.** With Drive MOVE and permission to delete local originals configured, the local maximum is a **soft limit**. When archiving is enabled, reaching that limit or the archive start threshold requests a background move through the existing archiver; when both are positive, the lower value is used. Uploads can temporarily exceed the limit while physical disk space covers other uploads’ outstanding reservations, temporary files, and a **100 MiB safety margin**. Admission does not wait for a Drive transfer to finish.

MOVE deletes only the corresponding local file after remote transfer, integrity verification, and index commit. Minimum age, pin/in-use protection, batch limits, and the shared cycle lock remain enforced. Drive outages and verification failures preserve originals and records. Uploads continue while physical headroom is sufficient; otherwise new uploads are rejected without deleting existing records. Disabling archiving prevents moves; configured Drive MOVE still uses the soft limit, with physical disk headroom controlling upload admission.

Moved recordings **remain listed, playable, and downloadable**. Automatic local reclamation never deletes Drive files or recordings referencing them, including COPY references. Only explicit **Completely delete** can enqueue remote deletion. Unattributed deletion intents from older versions are retained without automatic execution.

Outside Drive MOVE, the existing policy offers upload rejection or deletion of old unpinned segments after receipt and validation, never at reservation creation. Pins, active routes, and recordings referencing Drive are protected. See the [settings walkthrough](roadviewer_hars/DOCS.en.md#reading-the-storage-screen). Manual move/copy-all first shows target files, size, and exclusions for confirmation; see the [manual-run safeguards](roadviewer_hars/DOCS.en.md#manually-move-or-copy-all-eligible-data).


## Storage, backups and updates

Files reside under `/data/roadviewer-hars` inside the app container. **Home Assistant app backups exclude original logs, videos and prepared data.** Keep originals separately. Processing/storage policies and device authentication configuration are included.

An update that changes the analysis format may clear prepared data and reprocess recordings. Rendering-only updates do not inherently require reconversion.

The experimental **NAS archiving** option, enabled on the **저장공간** (Storage) tab, moves original videos and logs of converted recordings to network storage attached to Home Assistant with usage Share, freeing local space. It is off by default. Scheduled archiving moves pinned segments too (a manual move-all keeps them local), but NAS cleanup never removes them. The library and converted data stay local, an unreachable NAS never causes deletions, and there is no restore back to local storage. Files on the NAS are not part of Home Assistant backups. See [NAS archiving](roadviewer_hars/DOCS.en.md#nas-archiving-experimental) for setup.

**Google Drive** can be used as the archive instead of a NAS (experimental, off by default). Enter the ID and secret of an OAuth client of your own in the app options, then press **Google Drive 연결** (Connect Google Drive) on the Storage tab and enter the displayed code on Google's device sign-in page. **연결 해제** (Disconnect) removes only this installation's token; records, local files and Drive files are unchanged. It defaults to copy (COPY) mode: verified copies go to Drive and local files are never deleted. Move (MOVE) mode, which deletes local originals, applies only when the Storage tab sets MOVE together with **검증 후 로컬 원본 삭제 허용** (allow deleting local originals after verification); there is no automatic Drive cleanup. Videos moved to Drive play through the app, which holds only the video being watched (up to 256 MB) in memory so seeks stay quick; nothing is written to disk, and it is freed after 10 minutes unwatched. The sign-in token is part of Home Assistant backups, so keep backups safe. See [Google Drive archiving](roadviewer_hars/DOCS.en.md#google-drive-archiving-experimental).

Even with the same Drive account or NAS as Python 1.x, HARS 2.x writes only to its own location (the Drive folder `Road Viewer HARS`, a NAS sentinel owned by `roadviewer-hars`) and imports Python's archived records read-only. Real Google Drive and SMB mounts have not been validated yet ([sharing a Drive account or NAS](docs/hars/CUTOVER.md#같은-drive-계정nas를-함께-쓸-때), Korean).

## Documentation and validation

Screenshots were captured on 2026-11-07 from version 1.0.11 using example data, in light and dark mode; the one matching your theme is shown. The three driving/wide introduction photos composite the earlier footage's video area into the new UI. Device, capacity, and pairing values are synthetic; the displayed code cannot connect a device. See the [screenshot guide](docs/images/README.en.md). The screenshots were taken with Python 1.x.

- [User guide](roadviewer_hars/DOCS.en.md) · [한국어](roadviewer_hars/DOCS.md): installation, playback, route maps, conversion, storage, backups and troubleshooting
- [roadviewer-mcp](https://github.com/Qualified4/roadviewer-mcp): MCP server for AI agents, using only the existing Ingress login and API
- [Device API v1](docs/device-api.en.md) · [한국어](docs/device-api.md): HTTPS, pairing/signing, upload recovery and tests without openpilot
- [AppArmor security](docs/apparmor-security.en.md) · [한국어](docs/apparmor-security.md)
- [Historical Android picker analysis](docs/android-multiple-file-fix/README.en.md) · [한국어](docs/android-multiple-file-fix/README.md)
- [Current release notes](roadviewer_hars/CHANGELOG.en.md) · [Full Korean history](roadviewer_hars/CHANGELOG.md): 2.0.0 and the full Python 1.x history

The repository root is the Docker build context (`Dockerfile`). It builds the Rust executable `roadviewer-hars` onto `debian:bookworm-slim` and serves the HARS-owned web files (`web/`), initially copied from Python 1.3.18. The only runtime packages are `nginx` and `ca-certificates`; the image contains neither FFmpeg nor Python. GitHub Actions (**HARS CI**) runs the Rust checks (fmt, clippy, all tests), original ccnc client compatibility and upload fault injection, release-identity (`scripts/check_release.py`) and coexistence (`scripts/check_coexistence.py`) checks, and it builds the image on native amd64 and aarch64 runners with FFmpeg-absence, container boot and TLS checks. The required `HARS required` check also verifies the `Browser checks` result for `tests/browser` ([browser tests](docs/hars/BROWSER-TESTS.md), [development and testing](docs/hars/DEVELOPMENT.md), Korean).

The [Git history](https://github.com/Qualified4/roadviewer-hars/tree/d3de13472ea242dab9866d485b5777ad67d5b1dd) holds the Python 1.3.18 porting baseline. The build uses the openpilot schemas in `resources/schema/`, and CI tests the HARS executable with a pinned openpilot RV1 client.

### HARS technical documents (Korean)

- [Porting baseline feature and API inventory (Python 1.3.18)](docs/hars/inventory.json)
- [Implementation and validation status](docs/hars/STATUS.md)
- [Design and porting baseline](docs/hars/ARCHITECTURE.md)
- [HAOS side-by-side install, cutover and rollback](docs/hars/CUTOVER.md) · [Real HAOS validation preparation](docs/hars/HAOS-VALIDATION.md)
- [Running and testing](docs/hars/DEVELOPMENT.md)
- [Validation evidence](docs/hars/VALIDATION.md) · [Preliminary API measurements](docs/hars/PERFORMANCE.md)
- [Video format support](docs/hars/FORMAT-SUPPORT.md) · [FFmpeg removal](docs/hars/FFMPEG-REMOVAL.md)

### Development

Development run (Linux/WSL; needs Rust stable and `capnproto`. `ffmpeg` is not used by the product and is needed only for the tests' synthetic inputs and independent comparison):

```sh
cargo test --locked
HARS_DATA=/tmp/hars-dev HARS_INGRESS_ONLY=0 cargo run --locked
```

The UI listens on `http://127.0.0.1:8099` and the internal device API listener on `127.0.0.1:8098`. UI change requests need `X-RoadViewer-Request: 1` in development mode too. External HTTPS uses a separate Nginx listener. The device API is not exposed on the UI listener.

### Prebuilt release workflow

Publishing automation accepts stable version tags (`N.N.N`, for example `2.0.0`) and prerelease tags (`2.0.0-alpha.N`, `beta.N`, `rc.N`) ([stable release gate](docs/hars/ARCHITECTURE.md#정식-릴리스-게이트), Korean).

1. Set the version in `Cargo.toml`, `roadviewer_hars/config.yaml` and the `Dockerfile` `BUILD_VERSION` together; `scripts/check_release.py` checks that they match. Never reuse a published version. Web assets are cached for a year under versioned URLs, so the next publication must raise the version.
2. Confirm that `main` is a validated commit with passing CI, then push a tag named after the version (for example `2.0.0`) on that commit. The push starts `.github/workflows/release.yml` (**HARS release**).
3. The `gate` job runs `scripts/check_release.py --tag` (tag and version match). fmt, clippy and the tests are not run again: the commit passed them as `HARS required` in its pull request.
4. The `image` job builds on native amd64 and aarch64 runners and pushes `ghcr.io/qualified4/roadviewer-hars:<tag>-<arch>` only after the FFmpeg-absence check (`scripts/image_report.py --require-no-ffmpeg`) and `scripts/container_smoke.py` pass.
5. The `manifest` job combines both images as `ghcr.io/qualified4/roadviewer-hars:<tag>` and creates a GitHub release with `docs/hars/STATUS.md` as its notes: a prerelease when the tag has a `-` suffix, otherwise the stable, latest release. No `latest` image tag is published; Home Assistant pulls by version. If either architecture fails, no manifest or release is created.
6. Ensure the GHCR package is public; Home Assistant cannot pull a private image. Without credentials, `docker manifest inspect ghcr.io/qualified4/roadviewer-hars:<tag>` should list both architectures.

A published tag is never rewritten; fix problems in the next version. See [image build and publication](docs/hars/HAOS-VALIDATION.md#이미지-빌드게시-설정) (Korean) for details.

Road Viewer is distributed under the [MIT license](LICENSE). Raw driving logs/videos are not included in the repository; the introduction uses selected public screenshots. See [OPENPILOT-LICENSE](resources/schema/OPENPILOT-LICENSE) for the bundled openpilot schema license.
