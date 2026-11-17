package com.ccflash.app

import android.content.Intent
import android.os.Build
import android.os.Bundle
import androidx.activity.enableEdgeToEdge

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    // 本机 HTTP 服务由 Rust 线程启动; 前台服务只负责让进程在切后台/锁屏时
    // 不被 Doze 立即冻结(Android 12+ 从 Activity 前台启动不受限制)。
    val svc = Intent(this, LocalServerService::class.java)
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) startForegroundService(svc) else startService(svc)
    // Android 13+ 通知权限: 拒绝也不影响前台服务本身, 只是通知栏不显示
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
      try {
        requestPermissions(arrayOf(android.Manifest.permission.POST_NOTIFICATIONS), 0)
      } catch (_: Throwable) {
      }
    }
  }
}
