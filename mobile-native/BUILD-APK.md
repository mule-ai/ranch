# Building the Ranch Native APK

## Prerequisites

- Android SDK (platform 35, build-tools 35.0.0)
- JDK 17 (`mise where java`)
- Gradle 9.4.1 (via wrapper)
- Signing keystore at `~/.local/android-keystore/ranch.keystore`

## Build

```sh
cd mobile-native
export JAVA_HOME=$(mise where java)
export ANDROID_HOME=~/.local/android-sdk
export ANDROID_SDK_ROOT=$ANDROID_HOME
./gradlew assembleRelease
```

## Sign & verify

The release build uses the `ranch` keystore (configured in `app/build.gradle.kts`).
Verify with:

```sh
BT=$(ls -d ~/.local/android-sdk/build-tools/*/ | head -1)
"$BT/apksigner" verify app/build/outputs/apk/release/app-release.apk
```

## Deploy to releases/

```sh
cp app/build/outputs/apk/release/app-release.apk ../releases/ranch-native-0.2.0.apk
```

## Notes

- `applicationId` is `dev.ranch.android`.
- The app requires internet access to Supabase and the `POST_NOTIFICATIONS`
  permission (Android 13+).
- First run: sign in with email/password (your Supabase account),
  load machines, pick one, tap "Start monitoring".
