<template>
  <div class="app">
    <h1>idm-rs 下载管理器</h1>

    <div v-if="readOnly" class="banner">
      后台运行器正在运行，界面为只读。请先执行 idm-cli runner stop 后重启本程序。
    </div>

    <div v-if="fatalError" class="error-banner">{{ fatalError }}</div>

    <section class="add-bar">
      <input
        v-model="urlInput"
        :disabled="readOnly"
        type="text"
        placeholder="输入下载网址"
        @keyup.enter="onAdd"
      />
      <button :disabled="readOnly || !urlInput.trim()" @click="onAdd">添加</button>
      <div v-if="addError" class="error-text">{{ addError }}</div>
    </section>

    <table class="tasks">
      <thead>
        <tr>
          <th>文件名</th>
          <th>大小</th>
          <th>进度</th>
          <th>速度</th>
          <th>剩余时间</th>
          <th>状态</th>
          <th>操作</th>
        </tr>
      </thead>
      <tbody>
        <tr v-for="task in tasks" :key="task.id">
          <td class="name">{{ task.file_name || task.url_display }}</td>
          <td>{{ formatSize(task.total_size) }}</td>
          <td>
            <template v-if="task.total_size && task.total_size > 0">
              <progress :value="percent(task)" :max="100"></progress>
              <span class="pct">{{ percent(task) }}%</span>
            </template>
            <span v-else>{{ formatSize(task.downloaded) }}</span>
          </td>
          <td>{{ formatSpeed(task.speed_bps) }}</td>
          <td>{{ formatEta(task) }}</td>
          <td :title="task.state === 'Failed' ? (task.last_error || '') : ''">
            {{ stateText(task) }}
          </td>
          <td class="ops">
            <button
              :disabled="readOnly || !canPause(task)"
              @click="onPause(task.id)"
            >暂停</button>
            <button
              :disabled="readOnly || !canResume(task)"
              @click="onResume(task.id)"
            >继续</button>
            <button :disabled="readOnly" @click="askRemove(task)">删除</button>
          </td>
        </tr>
        <tr v-if="tasks.length === 0">
          <td colspan="7" class="empty">暂无任务</td>
        </tr>
      </tbody>
    </table>

    <div v-if="confirmTarget" class="overlay">
      <div class="dialog">
        <p>确定删除任务「{{ confirmTarget.file_name || confirmTarget.url_display }}」吗？</p>
        <label class="checkbox">
          <input v-model="deleteFile" type="checkbox" />
          同时删除已下载的文件
        </label>
        <div v-if="confirmError" class="error-text">{{ confirmError }}</div>
        <div class="dialog-ops">
          <button :disabled="readOnly" @click="confirmTarget = null">取消</button>
          <button :disabled="readOnly" @click="onConfirmRemove">确定</button>
        </div>
      </div>
    </div>

    <section class="settings">
      <button class="settings-toggle" @click="settingsOpen = !settingsOpen">
        {{ settingsOpen ? '收起设置' : '展开设置' }}
      </button>
      <div v-if="settingsOpen" class="settings-body">
        <div class="field">
          <label>下载目录</label>
          <input v-model="settings.download_dir" :disabled="readOnly" type="text" />
        </div>
        <div class="field">
          <label>全局限速（KB/秒，0 表示不限速）</label>
          <input v-model.number="speedKb" :disabled="readOnly" type="number" min="0" />
        </div>
        <div class="field">
          <label>同时下载任务数（1~16）</label>
          <input
            v-model.number="settings.max_tasks"
            :disabled="readOnly"
            type="number"
            min="1"
            max="16"
          />
        </div>
        <div class="field">
          <label>每任务默认连接数（1~32）</label>
          <input
            v-model.number="settings.default_connections"
            :disabled="readOnly"
            type="number"
            min="1"
            max="32"
          />
        </div>
        <button :disabled="readOnly" @click="onSaveSettings">保存</button>
        <div v-if="settingsError" class="error-text">{{ settingsError }}</div>
      </div>
    </section>
  </div>
</template>

<script setup lang="ts">
import { onMounted, onUnmounted, ref } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

interface TaskView {
  id: number;
  file_name: string | null;
  url_display: string;
  total_size: number | null;
  downloaded: number;
  speed_bps: number;
  state: string;
  pause_reason: string | null;
  last_error: string | null;
}

interface Settings {
  download_dir: string;
  global_max_speed: number;
  max_tasks: number;
  default_connections: number;
}

const readOnly = ref(false);
const tasks = ref<TaskView[]>([]);
const urlInput = ref('');
const addError = ref('');
const fatalError = ref('');
const settingsOpen = ref(false);
const settings = ref<Settings>({
  download_dir: '',
  global_max_speed: 0,
  max_tasks: 3,
  default_connections: 8,
});
const speedKb = ref(0);
const settingsError = ref('');
const confirmTarget = ref<TaskView | null>(null);
const deleteFile = ref(false);
const confirmError = ref('');

let unlisten: (() => void) | null = null;

function errText(err: unknown): string {
  if (typeof err === 'string') return err;
  if (err instanceof Error) return err.message;
  return String(err);
}

function formatSize(bytes: number | null): string {
  if (bytes === null || bytes === undefined) return '未知';
  const units = ['B', 'KB', 'MB', 'GB'];
  let value = bytes;
  let index = 0;
  while (value >= 1024 && index < units.length - 1) {
    value /= 1024;
    index += 1;
  }
  return `${value.toFixed(1)} ${units[index]}`;
}

function formatSpeed(bytesPerSecond: number): string {
  if (!bytesPerSecond || bytesPerSecond <= 0) return '—';
  return `${formatSize(bytesPerSecond)}/秒`;
}

function percent(task: TaskView): number {
  if (!task.total_size || task.total_size <= 0) return 0;
  const value = Math.round((task.downloaded / task.total_size) * 1000) / 10;
  return Math.max(0, Math.min(100, value));
}

function formatEta(task: TaskView): string {
  if (!task.total_size || !task.speed_bps || task.speed_bps <= 0) return '—';
  const remaining = task.total_size - task.downloaded;
  if (remaining <= 0) return '—';
  const totalSeconds = Math.ceil(remaining / task.speed_bps);
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;
  return `${hours}:${String(minutes).padStart(2, '0')}:${String(seconds).padStart(2, '0')}`;
}

function stateText(task: TaskView): string {
  switch (task.state) {
    case 'Queued':
      return '排队中';
    case 'Downloading':
      return '下载中';
    case 'Paused':
      return task.pause_reason === 'Interrupted' ? '已中断' : '已暂停';
    case 'Completed':
      return '已完成';
    case 'Failed':
      return '失败';
    default:
      return task.state;
  }
}

function canPause(task: TaskView): boolean {
  return task.state === 'Queued' || task.state === 'Downloading';
}

function canResume(task: TaskView): boolean {
  return task.state === 'Paused' || task.state === 'Failed';
}

async function refreshTasks(): Promise<void> {
  try {
    tasks.value = await invoke<TaskView[]>('list_tasks');
  } catch (err) {
    fatalError.value = errText(err);
  }
}

async function onAdd(): Promise<void> {
  addError.value = '';
  const url = urlInput.value.trim();
  if (!url) return;
  try {
    await invoke('add_task', { url, connections: null });
    urlInput.value = '';
    await refreshTasks();
  } catch (err) {
    addError.value = errText(err);
  }
}

async function onPause(id: number): Promise<void> {
  try {
    await invoke('pause_task', { id });
    await refreshTasks();
  } catch (err) {
    fatalError.value = errText(err);
  }
}

async function onResume(id: number): Promise<void> {
  try {
    await invoke('resume_task', { id });
    await refreshTasks();
  } catch (err) {
    fatalError.value = errText(err);
  }
}

function askRemove(task: TaskView): void {
  confirmTarget.value = task;
  deleteFile.value = false;
  confirmError.value = '';
}

async function onConfirmRemove(): Promise<void> {
  const target = confirmTarget.value;
  if (!target) return;
  confirmError.value = '';
  try {
    await invoke('remove_task', { id: target.id, deleteFile: deleteFile.value });
    confirmTarget.value = null;
    await refreshTasks();
  } catch (err) {
    confirmError.value = errText(err);
  }
}

async function onSaveSettings(): Promise<void> {
  settingsError.value = '';
  const payload: Settings = {
    download_dir: settings.value.download_dir,
    global_max_speed: Math.max(0, Math.round(speedKb.value)) * 1024,
    max_tasks: settings.value.max_tasks,
    default_connections: settings.value.default_connections,
  };
  try {
    await invoke('set_settings', { settings: payload });
    settings.value = payload;
  } catch (err) {
    settingsError.value = errText(err);
  }
}

onMounted(async () => {
  try {
    readOnly.value = await invoke<boolean>('is_read_only');
    const saved = await invoke<Settings>('get_settings');
    settings.value = saved;
    speedKb.value = Math.round(saved.global_max_speed / 1024);
    await refreshTasks();
    unlisten = await listen<string>('tasks-updated', (event) => {
      try {
        const parsed: unknown = JSON.parse(event.payload);
        if (Array.isArray(parsed)) {
          tasks.value = parsed as TaskView[];
        }
      } catch {
        // 忽略非法载荷，保留上一次的列表
      }
    });
  } catch (err) {
    fatalError.value = errText(err);
  }
});

onUnmounted(() => {
  if (unlisten) {
    unlisten();
    unlisten = null;
  }
});
</script>

<style>
.app {
  font-family: 'Microsoft YaHei', sans-serif;
  max-width: 960px;
  margin: 0 auto;
  padding: 16px;
  color: #222;
}
h1 {
  font-size: 20px;
  margin: 0 0 12px;
}
.banner {
  background: #fff3cd;
  border: 1px solid #ffeeba;
  color: #856404;
  padding: 10px;
  border-radius: 4px;
  margin-bottom: 12px;
}
.error-banner,
.error-text {
  color: #c00;
}
.error-banner {
  background: #f8d7da;
  border: 1px solid #f5c6cb;
  padding: 10px;
  border-radius: 4px;
  margin-bottom: 12px;
}
.add-bar {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
  align-items: center;
  margin-bottom: 16px;
}
.add-bar input[type='text'] {
  flex: 1;
  min-width: 240px;
  padding: 6px;
}
.add-bar .error-text {
  flex-basis: 100%;
}
button {
  padding: 5px 12px;
  cursor: pointer;
}
button:disabled {
  cursor: not-allowed;
  opacity: 0.5;
}
table.tasks {
  width: 100%;
  border-collapse: collapse;
  font-size: 13px;
}
table.tasks th,
table.tasks td {
  border: 1px solid #ddd;
  padding: 6px;
  text-align: left;
}
table.tasks th {
  background: #f5f5f5;
}
table.tasks .name {
  max-width: 200px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
table.tasks .ops {
  white-space: nowrap;
}
table.tasks .ops button {
  margin-right: 4px;
}
.pct {
  margin-left: 4px;
}
.empty {
  text-align: center;
  color: #888;
  padding: 16px;
}
.overlay {
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, 0.35);
  display: flex;
  align-items: center;
  justify-content: center;
}
.dialog {
  background: #fff;
  padding: 20px;
  border-radius: 6px;
  min-width: 320px;
}
.checkbox {
  display: flex;
  align-items: center;
  gap: 6px;
  margin: 12px 0;
}
.dialog-ops {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  margin-top: 12px;
}
.settings {
  margin-top: 20px;
  border-top: 1px solid #ddd;
  padding-top: 12px;
}
.settings-toggle {
  margin-bottom: 8px;
}
.settings-body {
  display: grid;
  gap: 10px;
  max-width: 480px;
}
.field label {
  display: block;
  margin-bottom: 4px;
}
.field input {
  width: 100%;
  box-sizing: border-box;
  padding: 6px;
}
</style>
