package com.ccflash.app

import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.provider.Settings
import android.view.View
import android.webkit.JavascriptInterface
import android.webkit.WebView
import android.widget.Toast
import androidx.activity.enableEdgeToEdge
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    // Android 15+ 默认 edge-to-edge: WebView 会画到状态栏/导航栏下面。
    // Android WebView 的 env(safe-area-inset-*) 并不可靠(常见为 0),
    // 设备面板/聊天头就会压住状态栏。这里把系统栏 + 刘海 insets 直接加到
    // 内容视图上, 让所有页面从安全区开始; 旋转/分屏时监听器会自动重算。
    val content = findViewById<View>(android.R.id.content)
    if (content != null) {
      ViewCompat.setOnApplyWindowInsetsListener(content) { v, insets ->
        val bars = insets.getInsets(
          WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout()
        )
        v.setPadding(bars.left, bars.top, bars.right, bars.bottom)
        insets
      }
      ViewCompat.requestApplyInsets(content)
    }
    startLocalService()
    ensureBatteryOptimizationExemption()
    // 运行时权限: 通知(Android 13+) + 本地网络(Android 16/17 的 LNP)。
    // 本地网络权限缺失时, 连接电脑地址、UDP 多播发现、上传下载会被系统拦截,
    // 表现为"一直连不上"; 拒绝也不崩溃, 用户可在系统设置里补授权。
    val perms = mutableListOf<String>()
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
      perms.add(android.Manifest.permission.POST_NOTIFICATIONS)
    }
    if (Build.VERSION.SDK_INT >= 36) {
      // 字符串形式: 低版本 compileSdk 下没有该常量也不影响编译
      perms.add("android.permission.ACCESS_LOCAL_NETWORK")
    }
    if (perms.isNotEmpty()) {
      try {
        requestPermissions(perms.toTypedArray(), 0)
      } catch (_: Throwable) {
      }
    }
  }

  override fun onRequestPermissionsResult(
    requestCode: Int,
    permissions: Array<out String>,
    grantResults: IntArray
  ) {
    super.onRequestPermissionsResult(requestCode, permissions, grantResults)
    if (requestCode != 0) return
    for (i in permissions.indices) {
      if (permissions[i] == "android.permission.ACCESS_LOCAL_NETWORK" &&
        grantResults.getOrNull(i) != PackageManager.PERMISSION_GRANTED) {
        // 拒绝后系统会拦掉局域网连接/发现, 界面只会表现为"连不上";
        // 首次拒绝必须明确告知用户去哪里补授权。
        Toast.makeText(
          this,
          "未授予本地网络权限：附近设备发现与电脑连接将不可用，请到系统设置 → 应用 → CC-Flash → 权限中开启",
          Toast.LENGTH_LONG
        ).show()
        return
      }
    }
  }

  /** WebView 创建时注入设备名读取接口: 聊天昵称优先用手机系统里显示的设备名称。 */
  override fun onWebViewCreate(webView: WebView) {
    super.onWebViewCreate(webView)
    try {
      webView.addJavascriptInterface(DeviceNameBridge(), "CCDevice")
    } catch (_: Throwable) {
      // 极少数 WebView 会禁用 JS 接口: 前端会退回到 UA/UA-CH 型号
    }
  }

  private inner class DeviceNameBridge {
    @JavascriptInterface
    fun getDeviceName(): String {
      try {
        if (Build.VERSION.SDK_INT >= 25) {
          val name = Settings.Global.getString(contentResolver, Settings.Global.DEVICE_NAME)
          if (!name.isNullOrBlank() && !name.equals("unknown", true) && !name.equals("Android", true)) {
            return name.trim()
          }
        }
      } catch (_: Throwable) {
      }
      val maker = Build.MANUFACTURER?.trim().orEmpty()
      val model = Build.MODEL?.trim().orEmpty()
      return when {
        maker.isEmpty() -> model
        model.isEmpty() -> maker
        model.startsWith(maker, ignoreCase = true) -> model
        else -> "$maker $model"
      }
    }
  }

  override fun onResume() {
    super.onResume()
    // Android 15+ dataSync 前台服务 6 小时配额耗尽后 onTimeout 会 stopSelf;
    // 回到仍存活的 Activity 时 onCreate 不会再走, 这里补一次拉起, 否则进程
    // 失去前台服务保护后会被 Doze/省电策略冻结, 传输与发现静默中断。
    startLocalService()
  }

  /**
   * 荣耀/华为/小米等系统会在锁屏后冻结后台应用(即使有前台服务): 典型表现是
   * 手机熄屏一段时间后, 桌面端发现不到它、发消息超时。首次启动申请"忽略电池优化";
   * 用户拒绝不影响前台使用, 只提示一次(记录在本地偏好)。
   */
  private fun ensureBatteryOptimizationExemption() {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.M) return
    try {
      val pm = getSystemService(android.os.PowerManager::class.java) ?: return
      if (pm.isIgnoringBatteryOptimizations(packageName)) return
      val prefs = getSharedPreferences("ccflash", MODE_PRIVATE)
      if (prefs.getBoolean("battery-opt-asked", false)) return
      prefs.edit().putBoolean("battery-opt-asked", true).apply()
      startActivity(
        Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS)
          .setData(android.net.Uri.parse("package:$packageName"))
      )
    } catch (_: Throwable) {
      // 部分厂商没有该设置页或限制直接申请: 保持前台服务 + Wi-Fi/CPU 锁降级运行
    }
  }

  /** 本机 HTTP 服务由 Rust 线程启动; 前台服务只负责让进程在后台不被立即冻结。 */
  private fun startLocalService() {
    try {
      val svc = Intent(this, LocalServerService::class.java)
      if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) startForegroundService(svc) else startService(svc)
    } catch (_: Throwable) {
      // 配额/后台限制等场景: 不崩溃; 用户回到前台时 onResume 会再次尝试
    }
  }
}
