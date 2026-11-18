plugins {
    alias(libs.plugins.android.application)
}

android {
    namespace = "studio.conspect.cast.tv"
    compileSdk = 37

    defaultConfig {
        applicationId = "studio.conspect.cast.tv"
        minSdk = 23
        targetSdk = 35
        versionCode = 1
        versionName = "0.1.0"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
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

    buildFeatures {
        buildConfig = true
    }

    lint {
        abortOnError = true
        warningsAsErrors = false
    }

    testOptions {
        unitTests.isReturnDefaultValues = true
    }
}


dependencies {
    implementation(libs.media3.exoplayer)
    implementation(libs.media3.exoplayer.hls)
    implementation(libs.media3.extractor)
    implementation(libs.media3.session)
    implementation(libs.media3.ui)
    implementation(libs.androidx.media)
    implementation(libs.play.services.cast.tv)
    implementation(libs.srtdroid.core)

    testImplementation(libs.junit)
    testImplementation(libs.org.json)

    androidTestImplementation(libs.androidx.test.ext.junit)
    androidTestImplementation(libs.androidx.test.runner)
}
