package com.ccflash.app

import android.content.Intent
import android.os.Build
import android.os.Bundle
import androidx.activity.enableEdgeToEdge

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    startLocalService()
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

  override fun onResume() {
    super.onResume()
    // Android 15+ dataSync 前台服务 6 小时配额耗尽后 onTimeout 会 stopSelf;
    // 回到仍存活的 Activity 时 onCreate 不会再走, 这里补一次拉起, 否则进程
    // 失去前台服务保护后会被 Doze/省电策略冻结, 传输与发现静默中断。
    startLocalService()
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
