plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android") version "2.0.21"
    id("org.mozilla.rust-android-gradle.rust-android")
}

android {
    namespace = "com.wl.android"
    compileSdk = 36

    defaultConfig {
        applicationId = "com.wl.android"
        minSdk = 33
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"
    }

    buildTypes {
        release {
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

cargo {
    module = "../native"
    libname = "land_native"
    targets = listOf("arm64-v8a")
    profile = "release"
}

dependencies {
    implementation("androidx.core:core-ktx:1.15.0")
}

tasks.whenTaskAdded {
    if (name.startsWith("merge") && name.endsWith("JniLibFolders")) {
        dependsOn("cargoBuild")
    }
}
