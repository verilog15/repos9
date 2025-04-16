#include "switch.hpp"
#include <wayfire/signal-definitions.hpp>
#include <wayfire/core.hpp>

wf::switch_device_t::switch_device_t(wlr_input_device *dev) :
    input_device_impl_t(dev)
{
    on_switch.set_callback([&] (void *data)
    {
        this->handle_switched((wlr_switch_toggle_event*)data);
    });
    on_switch.connect(&wlr_switch_from_input_device(dev)->events.toggle);
}

void wf::switch_device_t::handle_switched(wlr_switch_toggle_event *ev)
{
    wf::switch_signal data;
    data.device = nonstd::make_observer(this);
    data.state  = (ev->switch_state == WLR_SWITCH_STATE_ON);
    wf::get_core().emit(&data);
}
