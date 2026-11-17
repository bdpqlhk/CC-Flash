package com.ccflash.app

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.wifi.WifiManager
import android.os.Build
import android.os.IBinder
import androidx.annotation.RequiresApi

/**
 * 仅用于保活: 前台服务让进程在切后台/锁屏时不被 Doze 立即冻结。
 * 真正的网络监听与文件传输仍由 Rust 侧的本地 HTTP 服务完成。
 * MulticastLock 放在 Service 而不是 Activity: 系统回收 Activity 时
 * (后台内存压力 / 不保留活动) 多播发现不会静默失效。
 */
class LocalServerService : Service() {
  private var multicastLock: WifiManager.MulticastLock? = null

  override fun onCreate() {
    super.onCreate()
    acquireMulticastLock()
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
      val nm = getSystemService(NotificationManager::class.java)
      val ch = NotificationChannel(CHANNEL_ID, "本机服务", NotificationManager.IMPORTANCE_LOW)
      ch.setShowBadge(false)
      nm.createNotificationChannel(ch)
    }
    val builder = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
      Notification.Builder(this, CHANNEL_ID)
    } else {
      @Suppress("DEPRECATION")
      Notification.Builder(this)
    }
    val notif = builder
      .setContentTitle("CC-Flash 正在运行")
      .setContentText("本机文件服务与附近设备发现已就绪")
      .setSmallIcon(R.drawable.ic_stat_ccflash)
      .setOngoing(true)
      .build()
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
      startForeground(NOTIF_ID, notif, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
    } else {
      startForeground(NOTIF_ID, notif)
    }
  }

  /** Android 默认过滤发往本机的多播包; 持有 MulticastLock 才能收到附近设备。 */
  private fun acquireMulticastLock() {
    try {
      val wifi = applicationContext.getSystemService(WifiManager::class.java) ?: return
      multicastLock = wifi.createMulticastLock("ccflash-discovery").apply {
        setReferenceCounted(false)
        acquire()
      }
    } catch (_: Throwable) {
      // 无 Wi-Fi 硬件/权限时静默降级: HTTP 访问与手动地址仍然可用
    }
  }

  @RequiresApi(Build.VERSION_CODES.VANILLA_ICE_CREAM)
  override fun onTimeout(startId: Int) {
    // Android 15+ 对 dataSync 前台服务有 6h/24h 配额, 到点后系统要求主动停止。
    // 这里正常收尾; 用户下次回到前台时 Activity 会再次 startForegroundService。
    stopSelf(startId)
  }

  // 用户从最近任务划掉 App 时进程会被系统结束, 不自动复活, 避免"关不掉"
  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int = START_NOT_STICKY
  override fun onBind(intent: Intent?): IBinder? = null

  override fun onDestroy() {
    try { multicastLock?.release() } catch (_: Throwable) {}
    multicastLock = null
    super.onDestroy()
  }

  companion object {
    private const val CHANNEL_ID = "ccflash-service"
    private const val NOTIF_ID = 1001
  }
}
