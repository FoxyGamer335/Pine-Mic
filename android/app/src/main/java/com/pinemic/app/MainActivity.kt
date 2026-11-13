package dev.pinemic

import android.Manifest
import android.app.Activity
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.view.Gravity
import android.view.View
import android.widget.AdapterView
import android.widget.ArrayAdapter
import android.widget.Button
import android.widget.LinearLayout
import android.widget.Spinner
import android.widget.TextView

class MainActivity : Activity() {

    private lateinit var ipView: TextView
        private lateinit var statusView: TextView
            private lateinit var toggle: Button
                private lateinit var micSpinner: Spinner
                    private lateinit var sourceSpinner: Spinner

                        private var micOptions = listOf<MicDeviceOption>()

                        private fun dp(v: Int) = (v * resources.displayMetrics.density).toInt()

                            override fun onCreate(savedInstanceState: Bundle?) {
                                super.onCreate(savedInstanceState)

                                val root = LinearLayout(this).apply {
                                    orientation = LinearLayout.VERTICAL
                                    gravity = Gravity.CENTER_HORIZONTAL
                                    setPadding(dp(16), dp(24), dp(16), dp(24))
                                }

                                root.addView(TextView(this).apply {
                                    text = "Pine Mic"
                                    textSize = 26f
                                    gravity = Gravity.CENTER
                                })

                                ipView = TextView(this).apply {
                                    textSize = 18f
                                    gravity = Gravity.CENTER
                                    setPadding(0, dp(8), 0, dp(8))
                                }
                                root.addView(ipView)

                                // ---- Mic Device Selector ----
                                root.addView(TextView(this).apply {
                                    text = "Physical microphone:"
                                    textSize = 12f
                                    setPadding(0, dp(8), 0, dp(2))
                                })
                                micSpinner = Spinner(this)
                                root.addView(micSpinner)

                                // ---- Audio Source Selector ----
                                root.addView(TextView(this).apply {
                                    text = "Capture mode (AudioSource):"
                                    textSize = 12f
                                    setPadding(0, dp(8), 0, dp(2))
                                })
                                sourceSpinner = Spinner(this)
                                root.addView(sourceSpinner)

                                val sourceAdapter = ArrayAdapter(
                                    this,
                                    android.R.layout.simple_spinner_item,
                                    AUDIO_SOURCE_OPTIONS.map { it.label }
                                ).apply {
                                    setDropDownViewResource(android.R.layout.simple_spinner_dropdown_item)
                                }
                                sourceSpinner.adapter = sourceAdapter
                                sourceSpinner.setSelection(0) // Default to VOICE_RECOGNITION
                                sourceSpinner.onItemSelectedListener = object : AdapterView.OnItemSelectedListener {
                                    override fun onItemSelected(parent: AdapterView<*>?, view: View?, position: Int, id: Long) {
                                        val selected = AUDIO_SOURCE_OPTIONS[position]
                                        if (Streamer.selectedAudioSource != selected.source) {
                                            Streamer.selectedAudioSource = selected.source
                                            if (MicService.running) {
                                                MicService.restartCapture()
                                            }
                                        }
                                    }
                                    override fun onNothingSelected(parent: AdapterView<*>?) {}
                                }

                                statusView = TextView(this).apply {
                                    gravity = Gravity.CENTER
                                    setPadding(0, dp(12), 0, dp(16))
                                }
                                root.addView(statusView)

                                toggle = Button(this).apply {
                                    setOnClickListener {
                                        if (MicService.running) {
                                            startService(Intent(this@MainActivity, MicService::class.java).setAction(MicService.ACTION_STOP))
                                        } else {
                                            startStreaming()
                                        }
                                    }
                                }
                                root.addView(toggle)

                                root.addView(TextView(this).apply {
                                    text = "\nUSB: Enable USB debugging, plug in the cable, and click 'Connect USB' in the desktop app."
                                    gravity = Gravity.CENTER
                                    textSize = 11f
                                })

                                setContentView(root)
                            }

                            override fun onResume() {
                                super.onResume()
                                val ips = localIpv4Addresses()
                                ipView.text = if (ips.isEmpty()) "(no Wi-Fi connection)" else ips.joinToString("\n")

                                // Populate mic devices
                                micOptions = listMicDevices(this)
                                val micAdapter = ArrayAdapter(
                                    this,
                                    android.R.layout.simple_spinner_item,
                                    micOptions.map { it.label }
                                ).apply {
                                    setDropDownViewResource(android.R.layout.simple_spinner_dropdown_item)
                                }
                                micSpinner.adapter = micAdapter

                                // Restore selection index
                                val currentIdx = micOptions.indexOfFirst { it.id == Streamer.selectedDeviceId }
                                if (currentIdx >= 0) micSpinner.setSelection(currentIdx)

                                    micSpinner.onItemSelectedListener = object : AdapterView.OnItemSelectedListener {
                                        override fun onItemSelected(parent: AdapterView<*>?, view: View?, position: Int, id: Long) {
                                            if (position in micOptions.indices) {
                                                val selected = micOptions[position]
                                                if (Streamer.selectedDeviceId != selected.id) {
                                                    Streamer.selectedDeviceId = selected.id
                                                    if (MicService.running) {
                                                        MicService.restartCapture()
                                                    }
                                                }
                                            }
                                        }
                                        override fun onNothingSelected(parent: AdapterView<*>?) {}
                                    }

                                    MicService.statusListener = { s -> runOnUiThread { render(s) } }
                                    render(MicService.status)
                            }

                            override fun onPause() {
                                MicService.statusListener = null
                                super.onPause()
                            }

                            private fun render(status: String) {
                                statusView.text = status
                                toggle.text = if (MicService.running) "Stop" else "Start streaming"
                            }

                            private fun startStreaming() {
                                val needed = mutableListOf<String>()
                                if (checkSelfPermission(Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) {
                                    needed += Manifest.permission.RECORD_AUDIO
                                }
                                if (Build.VERSION.SDK_INT >= 33 &&
                                    checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
                                ) {
                                    needed += Manifest.permission.POST_NOTIFICATIONS
                                }
                                if (needed.isNotEmpty()) {
                                    requestPermissions(needed.toTypedArray(), 1)
                                    return
                                }
                                launchService()
                            }

                            private fun launchService() {
                                startForegroundService(Intent(this, MicService::class.java).setAction(MicService.ACTION_START))
                            }

                            override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) {
                                super.onRequestPermissionsResult(requestCode, permissions, grantResults)
                                if (checkSelfPermission(Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED) {
                                    launchService()
                                } else {
                                    statusView.text = "Microphone permission is required."
                                }
                            }
}
