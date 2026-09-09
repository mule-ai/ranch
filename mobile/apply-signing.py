#!/usr/bin/env python3
"""Re-apply the release signing config after `npx expo prebuild --platform android`
(the android/ dir is generated + gitignored). Personal keystore lives in
~/.local/android-keystore/ranch.keystore — see BUILD-APK.md."""
import pathlib, sys

g = pathlib.Path(__file__).parent / "android/app/build.gradle"
s = g.read_text()
if "signingConfigs.release" in s:
    print("signing config already applied")
    sys.exit(0)

old = """    signingConfigs {
        debug {"""
new = """    signingConfigs {
        release {
            // local personal-build keystore (not a store credential —
            // kept outside the repo in ~/.local/android-keystore)
            storeFile file(System.getenv("RANCH_KEYSTORE") ?: System.getProperty("user.home") + "/.local/android-keystore/ranch.keystore")
            storePassword 'ranch-sign-2026'
            keyAlias 'ranch'
            keyPassword 'ranch-sign-2026'
        }
        debug {"""
assert old in s, "debug signingConfig block not found"
s = s.replace(old, new, 1)

old2 = """        release {
            // Caution! In production, you need to generate your own keystore file.
            // see https://reactnative.dev/docs/signed-apk-android.
            signingConfig signingConfigs.debug"""
new2 = """        release {
            signingConfig signingConfigs.release"""
assert old2 in s, "release buildType block not found"
s = s.replace(old2, new2, 1)
g.write_text(s)
print("signing config applied to", g)
