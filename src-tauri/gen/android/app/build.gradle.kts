import java.util.Properties
import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("rust")
}

val tauriProperties = Properties().apply {
    val propFile = file("tauri.properties")
    if (propFile.exists()) {
        propFile.inputStream().use { load(it) }
    }
}

android {
    compileSdk = 37
    namespace = "com.ccflash.app"
    defaultConfig {
        // Tauri 移动壳经 localhost proxy 访问电脑服务；不直接放行任意私网明文。
        manifestPlaceholders["usesCleartextTraffic"] = "true"   // 局域网 HTTP 客户端: 访问电脑服务需明文
        applicationId = "com.ccflash.app"
        minSdk = 24
        targetSdk = 37
        versionCode = tauriProperties.getProperty("tauri.android.versionCode", "1").toInt()
        versionName = tauriProperties.getProperty("tauri.android.versionName", "1.0")
    }
    // 发布证书: 从 CI 注入的 CC_KEYSTORE_* 环境变量读取(见 .github/workflows/release.yml)。
    // 本地未设置这些变量时 storeFile 为空 → release 产出 unsigned 包, 与模板默认行为一致。
    signingConfigs {
        create("release") {
            val ks = System.getenv("CC_KEYSTORE_PATH")
            if (!ks.isNullOrEmpty()) {
                storeFile = file(ks)
                storePassword = System.getenv("CC_KEYSTORE_PASSWORD")
                keyAlias = System.getenv("CC_KEY_ALIAS")
                keyPassword = System.getenv("CC_KEY_PASSWORD")
            }
        }
    }
    buildTypes {
        getByName("debug") {
            manifestPlaceholders["usesCleartextTraffic"] = "true"   // 局域网 HTTP 客户端: 访问电脑服务需明文
            isDebuggable = true
            isJniDebuggable = true
            isMinifyEnabled = false
            packaging {
                jniLibs.keepDebugSymbols.add("*/arm64-v8a/*.so")
                jniLibs.keepDebugSymbols.add("*/armeabi-v7a/*.so")
                jniLibs.keepDebugSymbols.add("*/x86/*.so")
                jniLibs.keepDebugSymbols.add("*/x86_64/*.so")
            }
        }
        getByName("release") {
            // 局域网 HTTP 仅允许本机回环代理（后续接入 Tauri proxy/native relay）。
            manifestPlaceholders["usesCleartextTraffic"] = "true"   // 局域网 HTTP 客户端: 访问电脑服务需明文
            // 仅在 CC_KEYSTORE_PATH 有值时挂签名; 无证书时保持 null → 产出 unsigned 包,
            // 而不是 AGP "Keystore file not set" 直接失败
            val releaseSigning = signingConfigs.getByName("release")
            if (releaseSigning.storeFile != null) {
                signingConfig = releaseSigning
            }
            optimization {
               enable = true
            }
            proguardFiles(
                *fileTree(".") {
                  include("**/*.pro")
                  exclude("build/**")
                }.files.toTypedArray()
            )
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_1_8
        targetCompatibility = JavaVersion.VERSION_1_8
    }
    buildFeatures {
        buildConfig = true
    }
}

kotlin {
    compilerOptions {
        jvmTarget = JvmTarget.JVM_1_8
    }
}

rust {
    rootDirRel = "../../../"
}

dependencies {
    implementation("androidx.webkit:webkit:1.14.0")
    implementation("androidx.appcompat:appcompat:1.7.1")
    implementation("androidx.activity:activity-ktx:1.10.1")
    implementation("com.google.android.material:material:1.12.0")
    implementation("androidx.lifecycle:lifecycle-process:2.10.0")
    testImplementation("junit:junit:4.13.2")
    androidTestImplementation("androidx.test.ext:junit:1.1.4")
    androidTestImplementation("androidx.test.espresso:espresso-core:3.5.0")
}

apply(from = file("tauri.build.gradle.kts"))
