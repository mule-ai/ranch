plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "dev.ranch.android"
    compileSdk = 35

    defaultConfig {
        applicationId = "dev.ranch.android"
        minSdk = 26
        targetSdk = 35
        versionCode = 21
        versionName = "0.7.7"
        // baked by release.yml (same string as versions.json, e.g.
        // "main-82c0643"); "dev" locally → the update banner skips
        buildConfigField("String", "RANCH_VERSION", "\"${System.getenv("RANCH_VERSION") ?: "dev"}\"")
    }

    buildFeatures {
        buildConfig = true
    }

    signingConfigs {
        create("release") {
            val ksPath = System.getenv("RANCH_KEYSTORE")
                ?: "${System.getProperty("user.home")}/.local/android-keystore/ranch.keystore"
            val ksFile = File(ksPath)
            storeFile = ksFile
            storePassword = "ranch-sign-2026"
            keyAlias = "ranch"
            keyPassword = "ranch-sign-2026"
        }
    }

    buildTypes {
        release {
            // CI without the RANCH_KEYSTORE secret builds an unsigned APK
            // (matches the old RN apply-signing.py behavior)
            signingConfig = if (signingConfigs["release"].storeFile?.exists() == true)
                signingConfigs.getByName("release") else null
            isMinifyEnabled = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions {
        jvmTarget = "17"
    }
}

dependencies {
    implementation("com.squareup.okhttp3:okhttp:4.12.0")
    implementation("com.squareup.okio:okio:3.6.0")
}
