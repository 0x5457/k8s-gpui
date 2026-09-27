gpui_kit::actions!(
    k8s_shell,
    [Undo, Redo, Cut, Copy, Paste, SelectAll, RefreshView]
);

gpui_kit::actions!(
    k8s_app,
    [
        HideOthers,
        ShowAll,
        ToggleFullScreen,
        CheckForUpdates,
        RestartToUpdate
    ]
);
