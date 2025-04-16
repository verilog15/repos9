# frozen_string_literal: true

class Form::MultipleSelectComponent < ViewComponent::Base
  def initialize(form:, name:, options:, selected_items:, render_option_subtext: false, placeholder_term: nil, show_all_option: false)
    @form = form
    @name = name
    @options = options.to_json
    @selected_items = selected_items
    @render_option_subtext = render_option_subtext
    @placeholder_term = placeholder_term
    @show_all_option = show_all_option
  end
end
