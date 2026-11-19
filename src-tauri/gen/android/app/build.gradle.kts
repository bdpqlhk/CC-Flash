import java.io.File
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
    // 与 Rust 侧(NDK_HOME)使用同一份 NDK: 未指定时 AGP 会自行挑选另一版本,
    // 两边 NDK 不一致可能产出 ABI/符号异常的包(CI 镜像升级时尤易漂移)
    System.getenv("NDK_HOME")?.takeIf { it.isNotBlank() }?.let {
        ndkVersion = File(it).name
    }
    namespace = "com.ccflash.app"
    defaultConfig {
        // 移动壳是局域网 HTTP 客户端: 必须允许明文访问电脑端服务(局域网内无 TLS),
        // 与 res/xml/network_security_config.xml 的 cleartextTrafficPermitted=true 一致
        manifestPlaceholders["usesCleartextTraffic"] = "true"
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
            manifestPlaceholders["usesCleartextTraffic"] = "true"
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
            manifestPlaceholders["usesCleartextTraffic"] = "true"
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
    // APK 瘦身: 只保留中英文资源, 裁掉 AndroidX/Material 等依赖携带的其余语言
    // (AGP 9 的 localeFilters; 若旧 AGP 报错请改为 defaultConfig.resourceConfigurations)
    androidResources {
        localeFilters += listOf("zh", "en")
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
