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
        versionCode = 4
        versionName = "0.5.0"
    }

    signingConfigs {
        create("release") {
            storeFile = file(
                System.getenv("RANCH_KEYSTORE")
                    ?: "${System.getProperty("user.home")}/.local/android-keystore/ranch.keystore"
            )
            storePassword = "ranch-sign-2026"
            keyAlias = "ranch"
            keyPassword = "ranch-sign-2026"
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            signingConfig = signingConfigs.getByName("release")
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
