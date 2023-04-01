mod workspace_element;

use anyhow::{anyhow, Result};
use collections::HashMap;
use gpui::{
    actions, AnyViewHandle, AppContext, Entity, ModelHandle, MutableAppContext, Task, View,
    ViewContext, ViewHandle,
};
use project::{Project, ProjectItem, ProjectItemHandle, WorktreePath};
use std::{
    any::{Any, TypeId},
    path::PathBuf,
};

actions!(ws2, [CloseActivePaneItem]);

type PaneId = usize;

type BuildProjectPaneItem = Box<
    dyn Fn(
        ViewHandle<Workspace>,
        Box<dyn ProjectItemHandle>,
        &mut MutableAppContext,
    ) -> Box<dyn ProjectPaneItemHandle>,
>;
type ProjectPaneItemBuilders = HashMap<TypeId, BuildProjectPaneItem>;

type ConvertProjectPaneItemHandle = fn(AnyViewHandle) -> Box<dyn ProjectPaneItemHandle>;
type ProjectPaneItemHandleConverters = HashMap<TypeId, ConvertProjectPaneItemHandle>;

pub trait PaneItem: View {}

pub trait PaneItemHandle {
    fn to_project_pane_item(&self, cx: &AppContext) -> Option<Box<dyn ProjectPaneItemHandle>>;
    fn boxed_clone(&self) -> Box<dyn PaneItemHandle>;
}

impl<T: PaneItem> PaneItemHandle for ViewHandle<T> {
    fn to_project_pane_item(&self, cx: &AppContext) -> Option<Box<dyn ProjectPaneItemHandle>> {
        let converter = cx
            .global::<ProjectPaneItemHandleConverters>()
            .get(&TypeId::of::<T>())?;
        Some((converter)(self.into()))
    }

    fn boxed_clone(&self) -> Box<dyn PaneItemHandle> {
        Box::new(self.clone())
    }
}

pub trait ProjectPaneItem: PaneItem {
    type Model: ProjectItem;
    type Dependencies: Any;

    fn for_project_item(
        model: ModelHandle<Self::Model>,
        dependencies: &Self::Dependencies,
        cx: &mut ViewContext<Self>,
    ) -> Self;

    fn project_item(&self, cx: &AppContext) -> &ModelHandle<Self::Model>;
}

pub trait ProjectPaneItemHandle {
    fn project_item<'a>(&'a self, cx: &'a AppContext) -> &'a dyn ProjectItemHandle;
    fn as_pane_item(&self) -> &dyn PaneItemHandle;
    fn boxed_clone(&self) -> Box<dyn ProjectPaneItemHandle>;
}

impl<T: ProjectPaneItem> ProjectPaneItemHandle for ViewHandle<T> {
    fn project_item<'a>(&'a self, cx: &'a AppContext) -> &'a dyn ProjectItemHandle {
        self.read(cx).project_item(cx)
    }

    fn as_pane_item(&self) -> &dyn PaneItemHandle {
        self
    }

    fn boxed_clone(&self) -> Box<dyn ProjectPaneItemHandle> {
        Box::new(self.clone())
    }
}

pub struct Workspace {
    project: ModelHandle<Project>,
    pane_tree: PaneTree,
    next_pane_id: PaneId,
    active_pane_id: PaneId,
}

struct ProjectPaneItemRegistration {
    from_any: fn(AnyViewHandle) -> Option<Box<dyn ProjectPaneItemHandle>>,
}

enum SplitOrientation {
    Horizontal,
    Vertical,
}

enum PaneTree {
    Split {
        orientation: SplitOrientation,
        children: Vec<PaneTree>,
    },
    Pane(Pane),
}

pub struct Pane {
    id: PaneId,
    items: Vec<Box<dyn PaneItemHandle>>,
    active_item_index: usize,
}

pub fn init(cx: &mut MutableAppContext) {
    cx.set_global::<ProjectPaneItemBuilders>(Default::default());
    cx.set_global::<ProjectPaneItemHandleConverters>(Default::default());
    cx.add_action(Workspace::close_active_pane_item);
}

pub fn register_project_pane_item<T: ProjectPaneItem>(
    dependencies: T::Dependencies,
    cx: &mut MutableAppContext,
) {
    cx.update_global(|builders: &mut ProjectPaneItemBuilders, _| {
        builders.insert(
            TypeId::of::<T::Model>(),
            Box::new(move |workspace, model, cx| {
                Box::new(cx.add_view(workspace, |cx| {
                    T::for_project_item(model.to_any().downcast().unwrap(), &dependencies, cx)
                }))
            }),
        );
    });

    cx.update_global(|converters: &mut ProjectPaneItemHandleConverters, _| {
        converters.insert(TypeId::of::<T>(), |any_handle| {
            Box::new(any_handle.downcast::<T>().unwrap())
        });
    });
}

fn build_project_pane_item(
    project_item: Box<dyn ProjectItemHandle>,
    cx: &mut ViewContext<Workspace>,
) -> Result<Box<dyn ProjectPaneItemHandle>> {
    let workspace = cx.handle();
    cx.update_global(|builders: &mut ProjectPaneItemBuilders, cx| {
        let builder = builders
            .get(&project_item.item_type())
            .ok_or_else(|| anyhow!("no ProjectPaneItem registered for model type"))?;
        Ok(builder(workspace, project_item, cx))
    })
}

impl Entity for Workspace {
    type Event = ();
}

impl View for Workspace {
    fn ui_name() -> &'static str {
        "Workspace"
    }

    fn render(&mut self, _: &mut gpui::RenderContext<'_, Self>) -> gpui::ElementBox {
        todo!()
    }
}

impl Workspace {
    pub fn new(project: ModelHandle<Project>) -> Self {
        let pane_tree = PaneTree::new();
        Self {
            project,
            pane_tree,
            next_pane_id: 1,
            active_pane_id: 0,
        }
    }

    pub fn active_pane_mut(&mut self) -> &mut Pane {
        self.pane_tree.pane_mut(self.active_pane_id).unwrap()
    }

    pub fn open_abs_path(
        &self,
        abs_path: impl Into<PathBuf>,
        cx: &mut ViewContext<Self>,
    ) -> Task<Result<Box<dyn ProjectPaneItemHandle>>> {
        let worktree_path = self
            .project
            .update(cx, |project, cx| project.open_abs_path(abs_path, cx));

        cx.spawn(|this, mut cx| async move {
            let worktree_path = worktree_path.await?;
            let pane_item = this
                .update(&mut cx, |this, cx| this.open(worktree_path, cx))
                .await?;
            Ok(pane_item)
        })
    }

    pub fn open(
        &mut self,
        path: WorktreePath,
        cx: &mut ViewContext<Self>,
    ) -> Task<Result<Box<dyn ProjectPaneItemHandle>>> {
        let project_item = self
            .project
            .update(cx, |project, cx| project.open(path, cx));
        cx.spawn(|this, mut cx| async move {
            let project_item = project_item.await?;
            this.update(&mut cx, |this, cx| {
                let active_pane = this.active_pane_mut();
                if let Some(existing_item) =
                    active_pane.activate_project_item(project_item.as_ref(), cx)
                {
                    Ok(existing_item)
                } else {
                    let project_pane_item = build_project_pane_item(project_item, cx)?;
                    active_pane.add_item(project_pane_item.as_pane_item().boxed_clone(), cx);
                    Ok(project_pane_item)
                }
            })
        })
    }

    // Actions

    fn close_active_pane_item(&mut self, _: &CloseActivePaneItem, cx: &mut ViewContext<Self>) {
        if !self.active_pane_mut().close_active_item(cx) {
            cx.propagate_action(); // If pane was empty, there's no item to close
        }
    }
}

impl PaneTree {
    fn new() -> Self {
        PaneTree::Pane(Pane::new(0))
    }

    fn pane_mut(&mut self, pane_id: PaneId) -> Option<&mut Pane> {
        match self {
            PaneTree::Split { children, .. } => {
                for child in children {
                    if let Some(pane) = child.pane_mut(pane_id) {
                        return Some(pane);
                    }
                }
                None
            }
            PaneTree::Pane(pane) => {
                if pane.id == pane_id {
                    Some(pane)
                } else {
                    None
                }
            }
        }
    }
}

impl Pane {
    fn new(id: PaneId) -> Self {
        Self {
            id,
            items: Vec::new(),
            active_item_index: 0,
        }
    }

    /// If there's a pane item corresponding to the given project item handle,
    /// activate it and return a handle to it. Otherwise do nothing and return
    /// None.
    ///
    /// This helps us avoid opening multiple pane items for the same project
    /// item in a single pane.
    fn activate_project_item(
        &mut self,
        new_item: &dyn ProjectItemHandle,
        cx: &mut ViewContext<Workspace>,
    ) -> Option<Box<dyn ProjectPaneItemHandle>> {
        let new_entry_id = new_item.entry_id(cx)?;
        let (found_ix, found_item) =
            self.items.iter().enumerate().find_map(|(ix, pane_item)| {
                let project_pane_item = pane_item.to_project_pane_item(cx)?;
                let entry_id = project_pane_item.project_item(cx).entry_id(cx)?;
                if entry_id == new_entry_id {
                    Some((ix, project_pane_item))
                } else {
                    None
                }
            })?;

        self.active_item_index = found_ix;
        cx.notify();
        Some(found_item)
    }

    fn add_item(&mut self, item: Box<dyn PaneItemHandle>, cx: &mut ViewContext<Workspace>) {
        self.items.push(item);
        cx.notify();
    }

    fn close_active_item(&mut self, cx: &mut ViewContext<Workspace>) -> bool {
        if self.items.is_empty() {
            false
        } else {
            self.items
                .splice(self.active_item_index..self.active_item_index + 1, []);
            cx.notify();
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::{serde_json::json, TestAppContext};
    use project::Project;

    #[gpui::test]
    async fn test_ws2_workspace(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.background());
        fs.insert_tree(
            "/root1",
            json!({
                "a": ""
            }),
        )
        .await;

        let project = Project::test(fs, ["root1".as_ref()], cx).await;
        let (_, workspace) = cx.add_window(|cx| Workspace::new(project));

        let worktree_path = workspace
            .update(cx, |workspace, cx| workspace.open_abs_path("/root1", cx))
            .await;
    }
}
