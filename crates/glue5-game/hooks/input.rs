// Hooks for the runtime's `input.rs` (see ../hooks.rs).
&[
    // Once per frame for each of the 4 pads: feeds native gamepads (this build
    // has no XInput and never fills the DirectInput stick table).
    ("pad_update", "rage::ioPad::Update(bool)"),
    // The radio wheel's axes, which this game data maps to no device: it
    // reads the selection wheel's instead (and calls them here).
    ("radio_wheel_axes", "CRadioWheel::GetInputAxis(rage::ioValue const*&, rage::ioValue const*&) const"),
    ("selection_wheel_axes", "CSelectionWheel::GetInputAxis(rage::ioValue const*&, rage::ioValue const*&) const"),
    // A Scaleform method the scripts call, with its float arguments: the
    // phone's web browser gets mouse clicks it understands.
    ("scaleform_begin_method", "CScaleformMgr::BeginMethod(int, eSCALEFORM_BASE_CLASS, char const*, int)"),
    ("scaleform_add_param_float", "CScaleformMgr::AddParamFloat(double)"),
    // The game's text box (save names, the phone's cheat entry, ...): touch
    // devices show the system keyboard while it is open.
    ("text_box_open", "CTextInputBox::Open(rage::ioVirtualKeyboard::Params const&)"),
    // Its movie goes away however it closes: Close(), or its own update
    // when the text is accepted or cancelled (which does not call Close()).
    ("text_box_close", "CTextInputBox::RemoveTextInputBoxMovie()"),
]
