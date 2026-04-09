---
title: Parallel Agents - Zed
description: Run multiple agent threads concurrently using the Threads Sidebar, manage them across projects, and isolate work using git worktrees.
---

# Parallel Agents

Parallel Agents lets you run multiple agent threads at once, each working independently with its own agent, context window, and conversation history. The Threads Sidebar is the component where you start, manage, and switch between them.

Open the Threads Sidebar with {#kb multi_workspace::ToggleWorkspaceSidebar}.

## The Threads Sidebar {#threads-sidebar}

The sidebar shows your threads grouped by project. Each project gets its own section with a header. Threads appear below with their title, status indicator, and which agent is running them.

To focus the sidebar without toggling it, use {#kb multi_workspace::FocusWorkspaceSidebar}. To search your threads, press {#kb agents_sidebar::FocusSidebarFilter} while the sidebar is focused.

### Switching Threads {#switching-threads}

Click any thread in the sidebar to switch to it. The Agent Panel updates to show that thread's conversation.

For quick switching without opening the sidebar, use the thread switcher: press {#kb agents_sidebar::ToggleThreadSwitcher} to cycle forward through recent threads, or hold `Shift` while pressing that binding to go backward. This works from both the Agent Panel and the Threads Sidebar.

### The Archive {#archive}

The archive holds threads you've hidden or are no longer actively working in. Toggle the archive with {#kb agents_sidebar::ToggleArchive} or by clicking the archive icon in the sidebar bottom bar.

The archive has a search bar at the top. Type to search thread titles by fuzzy match.

### Importing ACP Threads {#importing-threads}

When the archive is open, an **Import ACP Threads** button appears in the sidebar bottom bar. Click it to open the import modal, which lists your installed external agents. Select the agents whose threads you want to import and confirm. Zed finds threads from those agents, whether they were started in Zed or in another client, and adds them to your thread history.

## Running Multiple Threads {#running-multiple-threads}

Start a new thread with {#action agent::NewThread}. Each thread runs independently, so you can send a prompt, open a second thread, and give it a different task while the first continues working.

To start a new thread scoped to the currently selected project in the sidebar, use {#action agents_sidebar::NewThreadInGroup}.

Each thread can use a different agent. Select the agent from the model selector in that thread's Agent Panel. You might run Zed's built-in agent in one thread and an [external agent](./external-agents.md) like Claude Code in another.

## Multiple Projects {#multiple-projects}

The Threads Sidebar can hold multiple projects at once. Each project gets its own group with its own threads and conversation history.

Within a project, you can add multiple folders. Use {#action workspace::AddFolderToProject} from the command palette, or select **Add Folder to Project** from the project header menu in the sidebar. Agents can then read and write across all of those folders in a single thread.

Agents have access to every project and folder in the sidebar, not just the one a thread was started in.

## Worktree Isolation {#worktree-isolation}

By default, all threads in a window share the same working copy. If two threads might edit the same files, start one in a new git worktree to give it an isolated checkout.

In the Agent Panel toolbar, click the worktree selector and choose **New Git Worktree**. You can also cycle between options with {#kb agent::CycleStartThreadIn}. When you send the first message, Zed creates a new worktree from the current branch and opens it in a new window.

After the agent finishes, review the diff, merge the changes through your normal git workflow, and delete the worktree when done.

> **Note:** Starting a thread in a new worktree requires the project to be in a git repository.

## Default Layout {#layout}

New installs place the Agent Panel and Threads Sidebar on the left. The Project Panel, Git Panel, and other panels move to the right, keeping the thread list and conversation next to each other. To rearrange panels, right-click any panel icon in the status bar.

## See Also {#see-also}

- [Agent Panel](./agent-panel.md): Manage individual threads and configure the agent
- [External Agents](./external-agents.md): Use Claude Code, Gemini CLI, and other agents
- [Tools](./tools.md): Built-in tools available in each thread
