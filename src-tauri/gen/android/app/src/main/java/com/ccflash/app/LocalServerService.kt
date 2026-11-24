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
import android.os.PowerManager
import androidx.annotation.RequiresApi

/**
 * 仅用于保活: 前台服务让进程在切后台/锁屏时不被 Doze 立即冻结。
 * 使用 connectedDevice 类型（局域网连接外部设备），而不是 dataSync：
 * Android 15+ 对 dataSync 有 6h/24h 配额，长时间后台会被系统停止；
 * connectedDevice 没有该时限。真正的网络监听与传输仍由 Rust 侧完成。
 * MulticastLock 放在 Service 而不是 Activity: 系统回收 Activity 时
 * (后台内存压力 / 不保留活动) 多播发现不会静默失效。
 */
class LocalServerService : Service() {
  private var multicastLock: WifiManager.MulticastLock? = null
  private var wifiLock: WifiManager.WifiLock? = null
  private var wakeLock: PowerManager.WakeLock? = null

  override fun onCreate() {
    super.onCreate()
    acquireMulticastLock()
    acquireBackgroundLocks()
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
      startForeground(NOTIF_ID, notif, ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE)
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

  /**
   * 熄屏保活: 荣耀/华为等系统的省电策略会在锁屏后冻结后台线程、让 Wi-Fi 休眠,
   * 表现为"手机放着不动, 桌面端就找不到它/发消息超时"。前台服务只保进程优先级,
   * 还需要:
   * - PARTIAL_WAKE_LOCK: 锁屏后 CPU 仍按时驱动发现心跳线程;
   * - WifiLock(LOW_LATENCY/HIGH_PERF): Wi-Fi 不进入省电休眠, 收发不被延迟。
   * 两把锁都不做引用计数, onDestroy 释放; 进程被杀时系统自动回收。
   */
  private fun acquireBackgroundLocks() {
    try {
      val wifi = applicationContext.getSystemService(WifiManager::class.java)
      val mode = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
        WifiManager.WIFI_MODE_FULL_LOW_LATENCY
      } else {
        @Suppress("DEPRECATION")
        WifiManager.WIFI_MODE_FULL_HIGH_PERF
      }
      wifiLock = wifi?.createWifiLock(mode, "ccflash-wifi")?.apply {
        setReferenceCounted(false)
        acquire()
      }
    } catch (_: Throwable) {
      // 无 Wi-Fi/权限受限: 降级为系统默认省电策略
    }
    try {
      val pm = getSystemService(PowerManager::class.java)
      wakeLock = pm?.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "ccflash:discovery")?.apply {
        setReferenceCounted(false)
        acquire()
      }
    } catch (_: Throwable) {
    }
  }

  @RequiresApi(Build.VERSION_CODES.VANILLA_ICE_CREAM)
  override fun onTimeout(startId: Int) {
    // 保留 onTimeout 兜底: 若厂商/系统仍对 connectedDevice 触发超时, 用可见通知告知用户。
    // 静默 stopSelf 会让用户完全无感(其他设备再也发现/连不上手机), 这里先发一条
    // 普通通知说明原因, 用户重新打开 App 时 onResume 会再次拉起前台服务。
    try {
      val nm = getSystemService(NotificationManager::class.java)
      val ch = NotificationChannel(ALERT_CHANNEL_ID, "服务提醒", NotificationManager.IMPORTANCE_DEFAULT)
      nm.createNotificationChannel(ch)
      val n = Notification.Builder(this, ALERT_CHANNEL_ID)
        .setContentTitle("CC Flash 后台保活已到系统时限")
        .setContentText("本机文件服务与附近设备发现已暂停，请重新打开 App 恢复")
        .setSmallIcon(R.drawable.ic_stat_ccflash)
        .setAutoCancel(true)
        .build()
      nm.notify(ALERT_NOTIF_ID, n)
    } catch (_: Throwable) {
      // 通知权限被拒/渠道异常: 不影响正常收尾
    }
    stopSelf(startId)
  }

  // 用户从最近任务划掉 App 时进程会被系统结束, 不自动复活, 避免"关不掉"
  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int = START_NOT_STICKY
  override fun onBind(intent: Intent?): IBinder? = null

  override fun onDestroy() {
    try { multicastLock?.release() } catch (_: Throwable) {}
    multicastLock = null
    try { wifiLock?.release() } catch (_: Throwable) {}
    wifiLock = null
    try { wakeLock?.release() } catch (_: Throwable) {}
    wakeLock = null
    super.onDestroy()
  }

  companion object {
    private const val CHANNEL_ID = "ccflash-service"
    private const val NOTIF_ID = 1001
    private const val ALERT_CHANNEL_ID = "ccflash-alert"
    private const val ALERT_NOTIF_ID = 1002
  }
}
