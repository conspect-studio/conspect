# Conspect Cast TV (Android TV receiver app)

Cast Connect receiver for Android TV and Google TV, `studio.conspect.cast.tv`. ADR-0114 slice 6 (app half),
decision 7. Two rungs: SRT (AES-256, MPEG-TS) and HLS. Not a Cargo workspace member.

Opened by hand it shows "Cast from Conspect to this TV" and the app version. Launched by a cast, it validates
the LOAD URL (https, `/cast/<token>/master.m3u8`, 43-char token, no credentials, query or fragment), asks
`POST /cast/<token>/ladder`, tries SRT (2.0 s to the first frame), else plays HLS and remembers the SRT failure
for that engine origin for 15 minutes, during which its launch checker declines Cast launches and the Web
Receiver loads. The token is held in memory only. The SRT passphrase is derived from the token and the one-time
`sid` (ADR decision 5.7) and set as a socket option, never in a URL.

## Build

JDK 17 or 21 and the Android SDK (platform android-37.0, build-tools 36.0.0; Gradle 9.6.0, AGP 9.5.1 and srtdroid 1.10.1 need compileSdk 37) are needed. Nothing is installed system-wide
by the repo; a local setup uses `.jdk/`, `.sdk/` and `.gradle-home/` (all gitignored):

```sh
export JAVA_HOME=$PWD/.jdk ANDROID_HOME=$PWD/.sdk GRADLE_USER_HOME=$PWD/.gradle-home
./gradlew --no-daemon assembleDebug testDebugUnitTest lint
# APK: app/build/outputs/apk/debug/app-debug.apk (debug-signed)
```

CI job `android-tv` runs the same command and uploads `conspect-cast-tv-debug.apk` (14 days). There is no
release signing here: the release key is operator step 8.

## Install

```sh
adb connect <tv-ip>:5555        # or USB debugging
adb install -r app-debug.apk
```

## Operator steps (device-only, ADR-0114 section 9)

1. Cast SDK Developer Console, application `8E05190C`: add the Android TV package `studio.conspect.cast.tv`.
2. Same Console: register the Cast serial number of each test device (the software serial in the device's Cast
   settings), then reboot it. A sideloaded app is launched only on a registered device.
3. On the device: enable developer options and debugging; install the APK with `adb install`.
4. Firewall: confirm UDP 9010 reaches the engine from the TV's network.
6. Device tests: SRT photograph on the Android TV device, "app not installed" and "SRT port blocked".
8. Release signing key: create it, store it in 1Password, add it as a CI secret.
10. Licence notices (libsrt MPL-2.0 source offer, Apache-2.0 notices) before the APK goes beyond test devices.
    Legal text is not written as engineering work.

## Device-only tests

- `app/src/androidTest/.../TsExtractorInstrumentedTest`: `TimestampAdjuster(MODE_NO_OFFSET)` keeps the wire PTS
  and unwraps the 33-bit cycle (needs an emulator or a TV; CI does not run it).
- Not written yet (device procedure): a synthetic TS through `TsExtractor` to the renderer's presentation time
  (`VideoFrameMetadataListener`), which gates the SRT `latency_ms` method; the reader stall; libsrt discarding
  rather than growing when the player stops reading; the launch, decline and fallback behaviour.

Until the first two pass on a device the SRT latency method stays `unavailable` and only `first_frame_ms`
is reported.

## Licences

| Component | Licence |
| --- | --- |
| libsrt 1.5.7 (shipped unmodified as a shared library; source: [Haivision/srt](https://github.com/Haivision/srt)) | MPL-2.0 |
| srtdroid (JNI binding) | Apache-2.0 |
| OpenSSL 3.5.1 (libsrt's crypto, built into srtdroid) | Apache-2.0 |
| AndroidX Media3 | Apache-2.0 |
| Google Play services Cast TV | Google's SDK terms (proprietary) |

No notice file is generated or edited here. The repo has no notice tooling for this app
(`tools/`, `scripts/`, `xtask/` have none); one that lists the dependency licences would need the Gradle
dependency list above as its input.
