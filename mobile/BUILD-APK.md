# Building the standalone APK (no Expo account, fully local)

One-time setup (all user-space, no sudo):

1. **JDK 17**: `mise use -g java@17.0.2` (already done for this machine)
2. **Android SDK**:
   ```sh
   mkdir -p ~/.local/android-sdk/cmdline-tools && cd ~/.local/android-sdk
   curl -sSL -o ct.zip https://dl.google.com/android/repository/commandlinetools-linux-11076708_latest.zip
   unzip -q ct.zip && mv cmdline-tools latest && rm ct.zip
   export ANDROID_HOME=~/.local/android-sdk
   yes | ~/.local/android-sdk/latest/bin/sdkmanager --sdk_root=$ANDROID_HOME --licenses
   ~/.local/android-sdk/latest/bin/sdkmanager --sdk_root=$ANDROID_HOME \
     "platform-tools" "platforms;android-36" "build-tools;36.0.0"
   ```
3. **Keystore**: `~/.local/android-keystore/ranch.keystore` (keytool PKCS12,
   alias `ranch`, pass `ranch-sign-2026` — personal build, not a store credential).

Every build:

```sh
cd mobile
npx expo prebuild --platform android --no-install
python3 apply-signing.py          # re-apply signing (android/ is generated)
export JAVA_HOME=$(mise where java) ANDROID_HOME=~/.local/android-sdk
cd android && ./gradlew assembleRelease   # ~7 min first time
cp app/build/outputs/apk/release/app-release.apk ../../releases/ranch-0.1.0.apk
```

The Supabase URL + anon key are compiled into the JS bundle
(`lib/config.ts` fallbacks) — no env needed for a personal build.
Install: copy the APK to the phone and open it (allow unknown sources).
